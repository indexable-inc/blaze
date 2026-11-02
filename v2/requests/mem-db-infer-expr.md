# mem-db-infer-expr: requests

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8). Files, all in
`crates/hir-ty/src/infer/`: `expr.rs`, `pat.rs`, `coerce.rs`, `callee.rs`, `cast.rs`, `op.rs`,
`place_op.rs`. Their owner is lane db (none done), so every change below is a request; apply
them together with `mem-db-infer-ctx`'s (`infer.rs`, `infer/autoderef.rs`), which these program
against. Lines are as of 2026-10-29 20:50 (UTC-7); other agents are editing these files, so each
entry quotes its "before" code to find it by. The `db` parameters (`&'db Db` now, `&dyn
HirDatabase` before) are left exactly as they are wherever they appear.

Notes and the speed-of-light bound: `docs/v2/plan/lanes/mem/db-infer-expr.md` (and "Speed" at the
end of this file).

## What these files allocate, in short

| temporary | made | before | after |
|---|---|---|---|
| coercion's adjustment list (`CoerceResult`) | per coercion | `Vec<Adjustment>` then `into_boxed_slice` (1 to 2 global) | `Vec<Adjustment, &'f Frame>`, copied once into the table; usually the frame's top block, so its drop frees it in place |
| every `write_expr_adj` argument | per adjusted expression | `Box<[Adjustment]>` (1 global; LUB: 1 per earlier arm) | `&[Adjustment]` (an array on the stack or a frame `Vec`) |
| autoderef's adjustment list (`adjust_steps_as_infer_ok`) | per field access, call, index, reborrow | `Vec<Adjustment>` (1 global) | `Vec<Adjustment, &'f Frame>` at exact length (ctx's function, interface X2) |
| `CoerceMany`'s pushed expressions | per `match`/block/loop/return site | `SmallVec<[ExprId; 4]>`, spills on a 5th arm (1 global per big `match`) | `Vec<ExprId, &'f Frame>`; `match` reserves its arm count once |
| `check_call_arguments`' formal/expected input lists | per call, method call, overloaded call | up to 4 global `Vec<Ty>` (collect, clone, two re-collecting folds in `fudge_inference_if_ok`, clone for the fallback) | 1 to 3 frame `Vec<Ty>`; the fold is in place (`FrameList`), the fallback borrows |
| tuple expression element types | per tuple expression | `Vec<Ty>` (1 global) | frame `Vec<Ty>` at exact length |
| struct literal field maps (`remaining_fields`, `seen_fields`, the two missing lists) | per struct literal | 2 `FxHashMap` (cloning a `Name` per field) + 2 `Vec` | frame `hashbrown` maps at exact capacity keyed by `&Name`, frame `Vec`s |
| record pattern field maps (`field_map`, `used_fields`, `inexistent_fields`, `unmentioned_fields`) | per record pattern | 2 `FxHashMap` + 2 `Vec` | frame maps at exact capacity keyed by `&Name`, frame `Vec`; the unmentioned list is made only for the diagnostic that owns it |
| `DeferredCallResolution::adjustments` | per call of a closure whose kind is unknown | `Vec<Adjustment>` | `Vec<Adjustment, &'f Frame>` (`DeferredCallResolution<'db, 'f>`) |
| `check_legacy_const_generics` | per call of a fn item | `generics()` and a `Vec<ConstParamId>` for every fn-item call, then `Box<[u32]>` | `&'f mut [u32]` in the frame; `generics()` and the frame `Vec<ConstParamId>` only for a function with legacy const generics |
| `CastCheck` | per `as` | 24 bytes in ctx's global `Vec<CastCheck>` | the same value in ctx's frame `Vec` (X4); `CastCheck` holds no heap data and gains no lifetime |
| `cast.rs`'s auto-trait set | per dyn-to-dyn pointer cast | `FxHashSet` | frame `Vec` with `contains` (a handful of auto traits) |
| `could_coerce` (entry point from `crates/hir`) | per IDE/diagnostic query | ran the coercion, then evaluated obligations, folded and re-collected the adjustments, all of which `.is_ok()` ignored | runs only the coercion, in `frame::task(Tag::Infer, ..)` |

Not present in this tree, though the brief names them: `coerce_unsized` has no queue (it checks
`CoerceUnsized`/`Unsize` with one `visit_proof_tree` and `CoerceVisitor`; the visitor's
`goal.candidates()` list is the solver's, mem-db-solver's). Pattern lowering has no binding-mode
stack (`PatInfo` is a `Copy` value passed down), slice patterns borrow their prefix and suffix
from the store (`before: &[PatId]`, `after: &[PatId]`), subpattern type lists are interned
`Tys`, and `PatOrigin` is a `Copy` enum. Those need no change.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): a summary of E0 to E17, each settled in its own section; in the tree (`crates/hir-ty/src/infer/{coerce,expr,pat,callee,cast,op,place_op}.rs`).

## Prerequisites (other parts)

- **mem-db-infer** (`lib.rs`, `Cargo.toml`): `#![feature(allocator_api)]` in
  `crates/hir-ty/src/lib.rs`; `blaze-mem.workspace = true` and `hashbrown.workspace = true` in
  `crates/hir-ty/Cargo.toml` (hashbrown's `nightly` feature is filed by mem-db, root request 3,
  and is what makes `hashbrown::HashMap<K, V, S, &Frame>` take `core::alloc::Allocator`).
- **mem-db-infer-ctx** (`infer.rs`): `pub(crate) frame: &'f blaze_mem::Frame` on
  `InferenceContext<'db, 'f>` (R2). Every `impl<'db> InferenceContext<'db>` header in my seven
  files becomes `impl<'db, 'f> InferenceContext<'db, 'f>`. That is entry E0 below, written out per
  file, so this file and ctx's can be applied in either order.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/lib.rs:5`, `crates/hir-ty/Cargo.toml:51`, `Cargo.toml` hashbrown `nightly`, and `InferenceContext<'db, 'f>`'s `pub(crate) frame` in `crates/hir-ty/src/infer.rs`.

## Cross-part interfaces these files program against

Each is a change in a file of another part. The exact code is given, so mem-db-infer can hand
it to that part's owner together with the part's own entries.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): X1 to X4 are settled in their sections below.

### X1. `write_expr_adj` takes `&[Adjustment]` (mem-db-infer-ctx, `infer.rs` about line 2281)

Every caller in my files builds a list only to hand it here, and the table copies it
(`CsrBuilder::insert` takes an iterator). A slice removes the `Box` each caller made. Before:

```rust
    pub(crate) fn write_expr_adj(&mut self, expr: ExprId, adjustments: Box<[Adjustment]>) {
        if adjustments.is_empty() {
            return;
        }
        let expr_adjustments = &mut self.result.expr_adjustments;
        if let Some(existing) = expr_adjustments.get_mut(&expr)
            && let [Adjustment { kind: Adjust::NeverToAny, target }] = &mut existing[..]
            && let [.., Adjustment { target: new_target, .. }] = &adjustments[..]
        {
            // NeverToAny coercion can target any type, so instead of adding a new
            // adjustment on top we can change the target.
            *target = new_target.clone();
            return;
        }
        expr_adjustments.insert(expr, adjustments);
    }
```

After:

```rust
    pub(crate) fn write_expr_adj(&mut self, expr: ExprId, adjustments: &[Adjustment]) {
        if adjustments.is_empty() {
            return;
        }
        let expr_adjustments = &mut self.result.expr_adjustments;
        if let Some(existing) = expr_adjustments.get_mut(&expr)
            && let [Adjustment { kind: Adjust::NeverToAny, target }] = &mut existing[..]
            && let [.., Adjustment { target: new_target, .. }] = adjustments
        {
            // NeverToAny coercion can target any type, so instead of adding a new
            // adjustment on top we can change the target.
            *target = *new_target;
            return;
        }
        expr_adjustments.insert(expr, adjustments.iter().cloned());
    }
```

(`StoredTy` is `Copy`, a `u32` index; `Adjustment` clones by value.) The one caller outside my
files, `crates/hir-ty/src/method_resolution.rs:168`
`self.write_expr_adj(receiver, result.adjustments);`, becomes
`self.write_expr_adj(receiver, &result.adjustments);` (mem-db-infer-method's file).

> forwarded to db (from requests, pass 16; told by tmux).

> applied by db-mem-infer-a-ctx (21:36): `infer.rs`'s `write_expr_adj` as written. The `method_resolution.rs:174` caller is lane items' file (frontend-fast-method holds it), filed in `v2/requests/db.md` "mem-db-infer-method, for lane items"; asked frontend-fast-method to write it.

### X2. `adjust_steps_as_infer_ok` builds its list in a frame (mem-db-infer-ctx, `infer/autoderef.rs:32`)

Before:

```rust
impl<'db, Ctx: AutoderefCtx<'db>> GeneralAutoderef<'db, Ctx> {
    pub(crate) fn adjust_steps_as_infer_ok(&mut self) -> InferOk<'db, Vec<Adjustment>> {
        let steps = self.steps();
        if steps.is_empty() {
            return InferOk { obligations: PredicateObligations::new(), value: vec![] };
        }

        let targets = steps.iter().skip(1).map(|&(ty, _)| ty).chain(iter::once(self.final_ty()));
        let steps: Vec<_> = steps
            .iter()
            .map(|&(_source, kind)| {
                if let AutoderefKind::Overloaded = kind {
                    Some(OverloadedDeref(Mutability::Not))
                } else {
                    None
                }
            })
            .zip(targets)
            .map(|(autoderef, target)| Adjustment {
                kind: Adjust::Deref(autoderef),
                target: target.store(),
            })
            .collect();

        InferOk { obligations: self.take_obligations(), value: steps }
    }
}
```

After (one frame allocation of exactly the step count, none when there are no steps):

```rust
impl<'db, Ctx: AutoderefCtx<'db>> GeneralAutoderef<'db, Ctx> {
    pub(crate) fn adjust_steps_as_infer_ok<'f>(
        &mut self,
        frame: &'f Frame,
    ) -> InferOk<'db, Vec<Adjustment, &'f Frame>> {
        let steps = self.steps();
        let mut adjustments = Vec::with_capacity_in(steps.len(), frame);
        if steps.is_empty() {
            return InferOk { obligations: PredicateObligations::new(), value: adjustments };
        }

        let targets = steps.iter().skip(1).map(|&(ty, _)| ty).chain(iter::once(self.final_ty()));
        adjustments.extend(
            steps
                .iter()
                .map(|&(_source, kind)| {
                    if let AutoderefKind::Overloaded = kind {
                        Some(OverloadedDeref(Mutability::Not))
                    } else {
                        None
                    }
                })
                .zip(targets)
                .map(|(autoderef, target)| Adjustment {
                    kind: Adjust::Deref(autoderef),
                    target: target.store(),
                }),
        );

        InferOk { obligations: self.take_obligations(), value: adjustments }
    }
}
```

with `use blaze_mem::Frame;`. The caller outside my files,
`crates/hir-ty/src/method_resolution/confirm.rs:194`
`self.ctx.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok());`, passes
`self.ctx.frame` (mem-db-infer-method's file). The autoderef's own `steps` and `obligations`
(`autoderef.rs`, `Steps: Default`, `PredicateObligations`) are mem-db-infer-method's and the
solver's.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/infer/autoderef.rs:33` (takes `frame: &'f Frame`; db-mem-infer-a-small, "already in place").

### X3. `InferenceContextAutoderef` carries `'f` (mem-db-infer-method, `autoderef.rs:135,167`)

R2: `InferenceContextAutoderefCtx<'a, 'db, 'f>(&'a mut InferenceContext<'db, 'f>)` and
`pub(crate) type InferenceContextAutoderef<'a, 'db, 'f, Steps = Vec<(Ty<'db>, AutoderefKind)>>`,
with `ctx(&mut self) -> &mut InferenceContext<'db, 'f>` and
`new_from_inference_context(ctx: &'a mut InferenceContext<'db, 'f>, ..)`. My files name it as
`InferenceContextAutoderef<'_, 'db, 'f>` (lifetimes in that order).

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/autoderef.rs:251` and `:262` (`<'a, 'db, 'f>`, `ctx() -> &mut InferenceContext<'db, 'f>`).

### X4. The context's fields these files fill (mem-db-infer-ctx, `infer.rs` about lines 1770 to 1800, 261, 2224, 2346)

```rust
    return_coercion: Option<DynamicCoerceMany<'db, 'f>>,
    breakables: Vec<BreakableContext<'db, 'f>, &'f Frame>,
    deferred_cast_checks: Vec<CastCheck<'db>, &'f Frame>,
    deferred_call_resolutions: Vec<(ExprId, DeferredCallResolution<'db, 'f>), &'f Frame>,
```

and `struct BreakableContext<'db, 'f> { .. coerce: Option<DynamicCoerceMany<'db, 'f>>, .. }`
(`find_breakable`/`find_continuable` take `&[BreakableContext<'_, '_>]`). In `InferenceContext::new`:
`breakables: Vec::new_in(frame)`, `deferred_cast_checks: Vec::new_in(frame)`,
`deferred_call_resolutions: Vec::new_in(frame)`. Then:

- `infer.rs:261` `let cast_checks = std::mem::take(&mut ctx.deferred_cast_checks);` becomes
  `let cast_checks = std::mem::replace(&mut ctx.deferred_cast_checks, Vec::new_in(ctx.frame));`
  (a frame `Vec` has no `Default`).
- `infer.rs:2224` `self.return_coercion = Some(CoerceMany::new(self.return_ty));` becomes
  `self.return_coercion = Some(CoerceMany::new(self.return_ty, self.frame));`, and
  `infer/closure.rs:326` `self.return_coercion.replace(CoerceMany::new(liberated_sig.output()));`
  becomes `self.return_coercion.replace(CoerceMany::new(liberated_sig.output(), self.frame));`
  (mem-db-infer-closure's file).
- `remove_deferred_call_resolutions` (`infer.rs:2346`, called from `infer/closure/analysis.rs:532`)
  returns a frame list:

```rust
    fn remove_deferred_call_resolutions(
        &mut self,
        closure_def_id: ExprId,
    ) -> Vec<DeferredCallResolution<'db, 'f>, &'f Frame> {
        let mut removed = Vec::new_in(self.frame);
        removed.extend(
            self.deferred_call_resolutions
                .extract_if(.., |(closure, _)| *closure == closure_def_id)
                .map(|(_, resolution)| resolution),
        );
        removed
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-closure (21:52): the `infer/closure.rs` `CoerceMany::new(liberated_sig.output(), self.frame)` line only; the rest of X4 is ctx's (A11).

> applied by db-mem-infer-a-ctx (21:36): `infer.rs`'s half (the four fields, `BreakableContext<'db, 'f>`, `find_breakable`/`find_continuable`, `infer_finalize`'s `mem::replace`, `collect_fn`'s `CoerceMany::new(self.return_ty, self.frame)`, `remove_deferred_call_resolutions`) as written; `infer/closure.rs:326` is db-mem-infer-a-closure's.

## E0. `impl` headers and `&mut InferenceContext` signatures (all seven files)

R2. Each `impl<'db> InferenceContext<'db> {` becomes `impl<'db, 'f> InferenceContext<'db, 'f> {`:
`expr.rs:60`, `pat.rs:242`, `callee.rs:38`, `op.rs:22`, `place_op.rs:28`, `coerce.rs:895`. Each
`&mut InferenceContext<'db>` in a signature becomes `&mut InferenceContext<'db, '_>` where the
item has no `'f` of its own: `cast.rs:127, 167, 266, 312, 462, 476, 492, 526` (the `CastCheck`
methods and `pointer_kind`) and `coerce.rs:1271, 1294, 1316, 1433` (the `CoerceMany` methods).
The ones that need the named `'f` are in the entries below (`InferenceCoercionDelegate`,
`DeferredCallResolution::resolve`, `demand_compatible`, the autoderef parameters).

Each file that names `Frame` adds `use blaze_mem::Frame;`; each that names a hashbrown map adds
`rustc_hash::FxBuildHasher` to its `rustc_hash` import.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): `expr.rs`, `pat.rs`, `callee.rs`, `cast.rs`, `op.rs`, `place_op.rs` (coerce.rs is db-mem-infer-a's); `cast.rs` has no `use blaze_mem::Frame;` (it names no `Frame`).

## coerce.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E1 to E5 in the tree (db-mem-infer-a), each checked below.

### E1. `Coerce` holds the frame; `CoerceResult` is a frame list (lines 88 to 220, 233 to 237, 340, 530 to 543)

Per coercion (every assignment, argument, `let` with a type, arm, block tail, return).

Before (88):

```rust
struct Coerce<D> {
    delegate: D,
    use_lub: bool,
```

After:

```rust
struct Coerce<'f, D> {
    delegate: D,
    frame: &'f Frame,
    use_lub: bool,
```

Before (106 to 120):

```rust
type CoerceResult<'db> = InferResult<'db, (Vec<Adjustment>, Ty<'db>)>;
...
fn success<'db>(
    adj: Vec<Adjustment>,
    target: Ty<'db>,
    obligations: PredicateObligations<'db>,
) -> CoerceResult<'db> {
```

After:

```rust
type CoerceResult<'db, 'f> = InferResult<'db, (Vec<Adjustment, &'f Frame>, Ty<'db>)>;
...
fn success<'db, 'f>(
    adj: Vec<Adjustment, &'f Frame>,
    target: Ty<'db>,
    obligations: PredicateObligations<'db>,
) -> CoerceResult<'db, 'f> {
```

`impl<'db, D> Coerce<D>` (123) becomes `impl<'db, 'f, D> Coerce<'f, D>`, and every
`-> CoerceResult<'db>` in it `-> CoerceResult<'db, 'f>` (`unify`, `unify_and`, `coerce`,
`coerce_from_inference_variable`, `coerce_borrowed_pointer`, `coerce_unsized`,
`coerce_from_safe_fn`, `coerce_from_fn_pointer`, `coerce_from_fn_item`, `coerce_closure_to_fn`,
`coerce_raw_ptr`).

`unify` (191 to 194), before:

```rust
        self.unify_raw(a, b)
            .and_then(|InferOk { value: ty, obligations }| success(vec![], ty, obligations))
```

after:

```rust
        let frame = self.frame;
        self.unify_raw(a, b).and_then(|InferOk { value: ty, obligations }| {
            success(Vec::new_in(frame), ty, obligations)
        })
```

`unify_and` (197 to 216), before:

```rust
        self.unify_raw(a, b).and_then(|InferOk { value: ty, obligations }| {
            success(
                adjustments
                    .into_iter()
                    .chain(std::iter::once(Adjustment {
                        target: ty.store(),
                        kind: final_adjustment,
                    }))
                    .collect(),
                ty,
                obligations,
            )
        })
```

after:

```rust
        let frame = self.frame;
        self.unify_raw(a, b).and_then(|InferOk { value: ty, obligations }| {
            let mut adjusted = Vec::new_in(frame);
            adjusted.extend(adjustments);
            adjusted.push(Adjustment { target: ty.store(), kind: final_adjustment });
            success(adjusted, ty, obligations)
        })
```

`coerce`'s never case (233 to 237), before:

```rust
                return success(
                    vec![Adjustment { kind: Adjust::NeverToAny, target: b.store() }],
                    b,
                    PredicateObligations::new(),
                );
```

after:

```rust
                let mut adjustments = Vec::with_capacity_in(1, self.frame);
                adjustments.push(Adjustment { kind: Adjust::NeverToAny, target: b.store() });
                return success(adjustments, b, PredicateObligations::new());
```

`coerce_from_inference_variable` (340) `success(vec![], target_ty, obligations)` becomes
`success(Vec::new_in(self.frame), target_ty, obligations)`; `coerce_borrowed_pointer` (530)
`return success(vec![], ty, obligations);` becomes
`return success(Vec::new_in(self.frame), ty, obligations);`.

`coerce_borrowed_pointer` (533 to 534), before:

```rust
        let InferOk { value: mut adjustments, obligations: o } =
            autoderef.adjust_steps_as_infer_ok();
```

after (X2; the `adjustments.push(..)` of the autoref that follows is unchanged):

```rust
        let InferOk { value: mut adjustments, obligations: o } =
            autoderef.adjust_steps_as_infer_ok(self.frame);
```

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/infer/coerce.rs:94` (`Coerce<'f, D>`) and `:113` (`CoerceResult<'db, 'f>` a `Vec<Adjustment, &'f Frame>`).

### E2. `InferenceCoercionDelegate` and `InferenceContext::coerce` (lines 867 to 926)

Before:

```rust
struct InferenceCoercionDelegate<'a, 'db>(&'a mut InferenceContext<'db>);

impl<'db> CoerceDelegate<'db> for InferenceCoercionDelegate<'_, 'db> {
```

After:

```rust
struct InferenceCoercionDelegate<'a, 'db, 'f>(&'a mut InferenceContext<'db, 'f>);

impl<'db> CoerceDelegate<'db> for InferenceCoercionDelegate<'_, 'db, '_> {
```

`InferenceContext::coerce` (about 908 to 925), before:

```rust
        let cause = ObligationCause::new(expr);
        let coerce_never = self.expr_guaranteed_to_constitute_read_for_never(expr, expr_is_read);
        let mut coerce = Coerce {
            delegate: InferenceCoercionDelegate(self),
            cause,
            allow_two_phase,
            coerce_never,
            use_lub: false,
        };
        let ok = coerce.commit_if_ok(|coerce| coerce.coerce(source, target))?;

        let (adjustments, _) = self.table.register_infer_ok(ok);
        self.write_expr_adj(expr, adjustments.into_boxed_slice());
        Ok(target)
```

after:

```rust
        let cause = ObligationCause::new(expr);
        let coerce_never = self.expr_guaranteed_to_constitute_read_for_never(expr, expr_is_read);
        let frame = self.frame;
        let mut coerce = Coerce {
            delegate: InferenceCoercionDelegate(self),
            frame,
            cause,
            allow_two_phase,
            coerce_never,
            use_lub: false,
        };
        let ok = coerce.commit_if_ok(|coerce| coerce.coerce(source, target))?;

        let (adjustments, _) = self.table.register_infer_ok(ok);
        self.write_expr_adj(expr, &adjustments);
        Ok(target)
```

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/infer/coerce.rs:736` and `:774` (`coerce` builds `Coerce` with `frame`, `write_expr_adj(expr, &adjustments)`).

### E3. `try_find_coercion_lub` (lines 945 to 1157)

Per LUB of two arms (each arm of a `match`/`if` after the first). Before, the fn-pointer case
boxed one adjustment per earlier arm, and the general case boxed a copy of the list per earlier
arm.

Before (1077 to 1089):

```rust
            for &expr in exprs {
                self.write_expr_adj(
                    expr,
                    Box::new([Adjustment {
                        kind: prev_adjustment.clone(),
                        target: fn_ptr.store(),
                    }]),
                );
            }
            self.write_expr_adj(
                new,
                Box::new([Adjustment { kind: next_adjustment, target: fn_ptr.store() }]),
            );
```

After:

```rust
            for &expr in exprs {
                self.write_expr_adj(
                    expr,
                    &[Adjustment { kind: prev_adjustment.clone(), target: fn_ptr.store() }],
                );
            }
            self.write_expr_adj(
                new,
                &[Adjustment { kind: next_adjustment, target: fn_ptr.store() }],
            );
```

Before (1101 to 1108):

```rust
        let mut coerce = Coerce {
            delegate: InferenceCoercionDelegate(self),
            cause: ObligationCause::new(new),
```

After:

```rust
        let frame = self.frame;
        let mut coerce = Coerce {
            delegate: InferenceCoercionDelegate(self),
            frame,
            cause: ObligationCause::new(new),
```

Before (1118) `self.write_expr_adj(new, adjustments.into_boxed_slice());`, after
`self.write_expr_adj(new, &adjustments);`. Before (about 1148)
`self.write_expr_adj(expr, adjustments.as_slice().into());`, after
`self.write_expr_adj(expr, &adjustments);`.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/infer/coerce.rs:847`, its `Coerce` with `frame` at `:1001`.

### E4. `CoerceMany` keeps its expressions in the frame (lines 1201 to 1242)

Per `match`, block, value-`loop`, `return` site. The `SmallVec<[ExprId; 4]>` spilled to the
global allocator on a fifth arm. In the frame, a `match` reserves its arm count once.

Before:

```rust
pub(crate) type DynamicCoerceMany<'db> = CoerceMany<'db, 'db>;

#[derive(Debug, Clone)]
enum Expressions<'exprs> {
    Dynamic(SmallVec<[ExprId; 4]>),
    UpFront(&'exprs [ExprId]),
}

impl<'db, 'exprs> CoerceMany<'db, 'exprs> {
    /// The usual case; collect the set of expressions dynamically.
    /// If the full set of coercion sites is known before hand,
    /// consider `with_coercion_sites()` instead to avoid allocation.
    pub(crate) fn new(expected_ty: Ty<'db>) -> Self {
        Self::make(expected_ty, Expressions::Dynamic(SmallVec::new()))
    }
```

After:

```rust
pub(crate) type DynamicCoerceMany<'db, 'f> = CoerceMany<'db, 'f>;

#[derive(Debug, Clone)]
enum Expressions<'exprs> {
    Dynamic(Vec<ExprId, &'exprs Frame>),
    UpFront(&'exprs [ExprId]),
}

impl<'db, 'exprs> CoerceMany<'db, 'exprs> {
    /// The usual case; collect the set of expressions dynamically.
    /// If the full set of coercion sites is known before hand,
    /// consider `with_coercion_sites()` instead to avoid allocation.
    pub(crate) fn new(expected_ty: Ty<'db>, frame: &'exprs Frame) -> Self {
        Self::make(expected_ty, Expressions::Dynamic(Vec::new_in(frame)))
    }

    /// [`Self::new`] with room for `capacity` expressions.
    pub(crate) fn with_capacity(
        expected_ty: Ty<'db>,
        capacity: usize,
        frame: &'exprs Frame,
    ) -> Self {
        Self::make(expected_ty, Expressions::Dynamic(Vec::with_capacity_in(capacity, frame)))
    }
```

`Vec<T, A>` is covariant in `A`, so a `CoerceMany<'db, 'f>` made with the run's frame is usable
where a shorter `'exprs` is. `coerce_inner` passes `exprs` (`&Vec<ExprId, &Frame>`) to
`try_find_coercion_lub(&[ExprId], ..)` by deref, unchanged; `buffer.push(expression)` is
unchanged. Remove `use smallvec::SmallVec;` (line 53, its only use).

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/infer/coerce.rs:1104` to `:1150` (`Expressions::Dynamic(Vec<ExprId, &'exprs Frame>)`, `new(.., frame)`, `with_capacity`).

### E5. `could_coerce` runs only what decides its answer, in a task (lines 1445 to 1625; R1 entry point)

Callers outside any query: `crates/hir/src/lib.rs:6169` (`Type::could_coerce_to`, through the
`hir_ide::could_coerce` re-export), reached from `blaze-diagnostics`'
`lower/{no_such_field,mismatched_arg_count,invalid_cast,type_mismatch,invalid_lhs_of_assignment}.rs`
while lowering diagnostics. It returned `coerce(db, env, tys).is_ok()`, and the private
`coerce` it called (its only caller) did the coercion with `?` first. Everything after the `?`
(registering and evaluating the obligations, the `Resolver` folder, re-collecting the
adjustments) could not change `is_ok()`. Running only the coercion keeps the answer for every
input. Before: `could_coerce` (1445 to 1451) plus `fn coerce<'db>(..)` with its `Resolver`
(1477 to 1625). After, replacing both (`HirCoercionDelegate` between them is unchanged):

```rust
pub fn could_coerce<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    tys: &Canonical<'db, (Ty<'db>, Ty<'db>)>,
) -> bool {
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let ((ty1_with_vars, ty2_with_vars), _) = infcx.instantiate_canonical(Span::Dummy, tys);

        // FIXME: Target features.
        let target_features = TargetFeatures::default();
        let mut coerce = Coerce {
            delegate: HirCoercionDelegate {
                infcx: &infcx,
                param_env: env.param_env,
                target_features: &target_features,
            },
            frame,
            cause: ObligationCause::dummy(),
            allow_two_phase: AllowTwoPhase::No,
            coerce_never: true,
            use_lub: false,
        };
        coerce.coerce(ty1_with_vars, ty2_with_vars).is_ok()
    })
}
```

(keep the `db` line as the tree has it then.) The imports only the removed code used go: from
`rustc_type_ir::{..}` remove `BoundVar, DebruijnIndex, TypeFoldable, TypeFolder,
TypeSuperFoldable, TypeVisitableExt` and `GenericArg as _` from `inherent::{..}`; from
`next_solver::{..}` remove `BoundConst, BoundRegion, BoundRegionKind, BoundTy, BoundTyKind,
Const, ConstKind, ErrorGuaranteed, RegionKind`. (Checked by `rg` on the file: each is used only
in `Resolver`; `has_infer` was the only `TypeVisitableExt` method.) What remains global here is
the solver's `InferCtxt` (mem-db-solver's).

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): in the tree at `crates/hir-ty/src/infer/coerce.rs:1353` and `:1358` (`frame::task(Tag::Infer, ..)`).

## expr.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E6 to E10 applied below (db-mem-infer-a-expr), in the tree.

### E6. Match arms reserve their count; breakables take the frame (lines 514, 2291 to 2302)

Before (514): `let mut coerce = CoerceMany::new(result_ty);`
After: `let mut coerce = CoerceMany::with_capacity(result_ty, arms.len(), self.frame);`

`with_breakable_ctx`, before:

```rust
    ) -> (BreakableContext<'db>, T) {
        self.breakables.push({
            BreakableContext { kind, may_break: false, coerce: ty.map(CoerceMany::new), label }
        });
```

after:

```rust
    ) -> (BreakableContext<'db, 'f>, T) {
        let frame = self.frame;
        self.breakables.push({
            BreakableContext {
                kind,
                may_break: false,
                coerce: ty.map(|ty| CoerceMany::new(ty, frame)),
                label,
            }
        });
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): as written.

### E7. Tuple expression element types (lines 777 to 796)

Per tuple expression. Before:

```rust
            Expr::Tuple { exprs, .. } => {
                let mut tys = match expected
                    .only_has_type(&mut self.table)
                    .map(|t| self.table.try_structurally_resolve_type(tgt_expr.into(), t).kind())
                {
                    Some(TyKind::Tuple(substs)) => substs
                        .iter()
                        .chain(repeat_with(|| self.table.next_ty_var(Span::Dummy)))
                        .take(exprs.len())
                        .collect::<Vec<_>>(),
                    _ => exprs.iter().map(|&expr| self.table.next_ty_var(expr.into())).collect(),
                };
```

After:

```rust
            Expr::Tuple { exprs, .. } => {
                let mut tys = Vec::with_capacity_in(exprs.len(), self.frame);
                match expected
                    .only_has_type(&mut self.table)
                    .map(|t| self.table.try_structurally_resolve_type(tgt_expr.into(), t).kind())
                {
                    Some(TyKind::Tuple(substs)) => tys.extend(
                        substs
                            .iter()
                            .chain(repeat_with(|| self.table.next_ty_var(Span::Dummy)))
                            .take(exprs.len()),
                    ),
                    _ => tys.extend(exprs.iter().map(|&expr| self.table.next_ty_var(expr.into()))),
                }
```

(the loop and `Ty::new_tup(self.interner(), &tys)` after it are unchanged).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): as written.

### E8. Struct literal field maps (lines 1120 to 1190)

Per struct literal. Before:

```rust
        let mut remaining_fields = variant_fields
            .fields()
            .iter()
            .map(|(i, field)| (field.name.clone(), i))
            .collect::<FxHashMap<_, _>>();

        let mut seen_fields = FxHashMap::default();
```

After (`variant_fields` is the database's `&'db VariantFields`, so the keys borrow from it and no
`Name` is copied; `remove(name)`, `insert(name, i)` and `get(&name)` keep their text, through
`&Name: Borrow<Name>`):

```rust
        let mut remaining_fields = hashbrown::HashMap::with_capacity_and_hasher_in(
            variant_fields.len(),
            FxBuildHasher,
            self.frame,
        );
        remaining_fields.extend(variant_fields.fields().iter().map(|(i, field)| (&field.name, i)));

        let mut seen_fields =
            hashbrown::HashMap::with_capacity_and_hasher_in(hir_fields.len(), FxBuildHasher, self.frame);
```

Before (1181 to 1182):

```rust
                let mut missing_mandatory_fields = Vec::new();
                let mut missing_optional_fields = Vec::new();
```

After:

```rust
                let mut missing_mandatory_fields = Vec::new_in(self.frame);
                let mut missing_optional_fields = Vec::new_in(self.frame);
```

Imports: `use rustc_hash::FxHashMap;` (24, used only here) becomes `use rustc_hash::FxBuildHasher;`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): as written.

### E9. `lookup_field` and its two callers (lines 1716 to 1792, 1815, 1938)

Per field access and per unresolved method call. Before:

```rust
    ) -> Option<(Ty<'db>, Either<FieldId, TupleFieldId>, Vec<Adjustment>, bool)> {
        let interner = self.interner();
```

After:

```rust
    ) -> Option<(Ty<'db>, Either<FieldId, TupleFieldId>, Vec<Adjustment, &'f Frame>, bool)> {
        let interner = self.interner();
        let frame = self.frame;
```

Its two `self.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok());` become
`self.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok(frame));` (X2). The callers'
`self.write_expr_adj(receiver, adjustments.into_boxed_slice());` (1815, 1938) become
`self.write_expr_adj(receiver, &adjustments);`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): as written; also mem-db-infer-ctx A4's caller in `lookup_field`, `TupleId(self.tuple_field_accesses_rev.insert(substs))`.

### E10. `check_call_arguments`' input lists (lines 2050 to 2285)

Per call, method call and overloaded call. The main temporary: before, up to four global
`Vec<Ty>`: the collected formal list, its clone inside `fudge_inference_if_ok`, which
`TypeFoldable for Vec<T>` (`vendor/rustc_type_ir/src/fold.rs:364`) re-collects on each of the
fudge's two folds, and another clone for the fallback.

New, in `expr.rs` beside `ExprIsRead`:

```rust
/// A frame list of foldable values, folded in place: `fudge_inference_if_ok` folds what its
/// closure returns, and `TypeFoldable for Vec<T>` collects a new list in the global allocator.
#[derive(Clone, Debug)]
struct FrameList<'f, T>(Vec<T, &'f Frame>);

impl<I: Interner, T: TypeVisitable<I>> TypeVisitable<I> for FrameList<'_, T> {
    fn visit_with<V: TypeVisitor<I>>(&self, visitor: &mut V) -> V::Result {
        for item in &self.0 {
            try_visit!(item.visit_with(visitor));
        }
        V::Result::output()
    }
}

impl<I: Interner, T: TypeFoldable<I>> TypeFoldable<I> for FrameList<'_, T> {
    fn try_fold_with<F: FallibleTypeFolder<I>>(mut self, folder: &mut F) -> Result<Self, F::Error> {
        for item in &mut self.0 {
            *item = item.clone().try_fold_with(folder)?;
        }
        Ok(self)
    }

    fn fold_with<F: TypeFolder<I>>(mut self, folder: &mut F) -> Self {
        for item in &mut self.0 {
            *item = item.clone().fold_with(folder);
        }
        self
    }
}
```

with `use rustc_type_ir::{FallibleTypeFolder, TypeFoldable, TypeFolder, TypeVisitable,
TypeVisitor, VisitorResult, try_visit};` added to the existing `rustc_type_ir` import (`Interner`
is already there). It folds each element in order and stops at the first error, as
`Vec<T>`'s does, so `fudge_inference_if_ok` returns the same values.

Before (2066 to 2073):

```rust
        let formal_input_tys: Vec<_> = formal_input_tys
            .iter()
            .map(|&ty| {
                let generalized_ty = self.table.next_ty_var(call_expr.into());
                let _ = self.demand_eqtype(call_expr.into(), ty, generalized_ty);
                generalized_ty
            })
            .collect();
```

After:

```rust
        let frame = self.frame;
        let formal_input_tys = {
            let mut generalized = Vec::with_capacity_in(formal_input_tys.len(), frame);
            for &ty in formal_input_tys {
                let generalized_ty = self.table.next_ty_var(call_expr.into());
                let _ = self.demand_eqtype(call_expr.into(), ty, generalized_ty);
                generalized.push(generalized_ty);
            }
            generalized
        };
```

Before (2080, 2110, 2113):

```rust
        let expected_input_tys: Option<Vec<_>> = expectation
...
                        Ok(Some(formal_input_tys.clone()))
                    })
                    .ok()
            })
            .unwrap_or_default();
```

After (the fudge returns `Option<FrameList>`, folded in place):

```rust
        let expected_input_tys: Option<Vec<Ty<'db>, &Frame>> = expectation
...
                        Ok(Some(FrameList(formal_input_tys.clone())))
                    })
                    .ok()
            })
            .unwrap_or_default()
            .map(|list| list.0);
```

Before (2126 to 2144):

```rust
                    let expected_input_tys = match expected_input_tys {
                        Some(expected_input_tys) => match expected_input_tys.first() {
                            Some(ty) => match ty.kind() {
                                TyKind::Tuple(tys) => Some(tys.iter().collect()),
                                _ => None,
                            },
                            None => None,
                        },
                        None => None,
                    };
                    (arg_types.to_vec(), expected_input_tys)
                }
                _ => {
                    // Otherwise, there's a mismatch, so clear out what we're expecting, and set
                    // our input types to err_args so we don't blow up the error messages
                    // FIXME: Emit an error.
                    (vec![self.types.types.error; provided_args.len()], None)
                }
```

After:

```rust
                    let expected_input_tys = match expected_input_tys {
                        Some(expected_input_tys) => match expected_input_tys.first() {
                            Some(ty) => match ty.kind() {
                                TyKind::Tuple(tys) => {
                                    let mut list = Vec::with_capacity_in(tys.len(), frame);
                                    list.extend_from_slice(tys.as_slice());
                                    Some(list)
                                }
                                _ => None,
                            },
                            None => None,
                        },
                        None => None,
                    };
                    let mut formal = Vec::with_capacity_in(arg_types.len(), frame);
                    formal.extend_from_slice(arg_types.as_slice());
                    (formal, expected_input_tys)
                }
                _ => {
                    // Otherwise, there's a mismatch, so clear out what we're expecting, and set
                    // our input types to err_args so we don't blow up the error messages
                    // FIXME: Emit an error.
                    let mut formal = Vec::with_capacity_in(provided_args.len(), frame);
                    formal.resize(provided_args.len(), self.types.types.error);
                    (formal, None)
                }
```

Before (2149 to 2156), the fallback that cloned:

```rust
        let expected_input_tys = if let Some(expected_input_tys) = expected_input_tys {
            assert_eq!(expected_input_tys.len(), formal_input_tys.len());
            expected_input_tys
        } else {
            formal_input_tys.clone()
        };
```

After (borrows; both are read only from here on):

```rust
        let expected_input_tys: &[Ty<'db>] = match &expected_input_tys {
            Some(expected_input_tys) => {
                assert_eq!(expected_input_tys.len(), formal_input_tys.len());
                expected_input_tys
            }
            None => &formal_input_tys,
        };
```

Before (2183) `let demand_compatible = |this: &mut InferenceContext<'db>, idx| {`, after
`let demand_compatible = |this: &mut InferenceContext<'db, 'f>, idx: usize| {` (its body indexes
both lists as before).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): as written, on the function as db-ty-d-infer1 left it: `demand_compatible` now also takes `provided_arg: ExprId` (kept); `call_arguments(.., &formal_input_tys, &expected_input_tys, ..)` reads the frame list and the borrowed slice unchanged. `TypeFoldable` was already imported, so only the other six names were added; the three new `.clone()`s carry `// clone:` notes for the hot lint.

## pat.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E11 applied below (db-mem-infer-a-expr), in the tree.

### E11. Record pattern field maps (lines 1158 to 1257)

Per record pattern. Before (1175 to 1200):

```rust
        let variant_fields = variant.fields(self.db);
        let field_map = variant_fields
            .fields()
            .iter()
            .map(|(i, field)| (field.name.clone(), i))
            .collect::<FxHashMap<_, _>>();
        let variant_field_tys = self.db.field_types(variant);
        let variant_fields_vis = VariantFields::field_visibilities(self.db, variant);

        // Keep track of which fields have already appeared in the pattern.
        let mut used_fields = FxHashMap::default();

        let mut inexistent_fields = vec![];
        // Typecheck each field.
        for (field_idx, field) in fields.iter().enumerate() {
            match used_fields.entry(field.name.clone()) {
```

After (keys borrow the database's `VariantFields` and the store's patterns; no `Name` copied):

```rust
        let variant_fields = variant.fields(self.db);
        let mut field_map = hashbrown::HashMap::with_capacity_and_hasher_in(
            variant_fields.len(),
            FxBuildHasher,
            self.frame,
        );
        field_map.extend(variant_fields.fields().iter().map(|(i, field)| (&field.name, i)));
        let variant_field_tys = self.db.field_types(variant);
        let variant_fields_vis = VariantFields::field_visibilities(self.db, variant);

        // Keep track of which fields have already appeared in the pattern.
        let mut used_fields =
            hashbrown::HashMap::with_capacity_and_hasher_in(fields.len(), FxBuildHasher, self.frame);

        let mut inexistent_fields = Vec::new_in(self.frame);
        // Typecheck each field.
        for (field_idx, field) in fields.iter().enumerate() {
            match used_fields.entry(&field.name) {
```

(`Occupied`/`Vacant` and `field_map.get(&field.name)` keep their text.)

Before (1222 to 1257), the unmentioned list made for every record pattern:

```rust
        let unmentioned_fields = variant_fields
            .fields()
            .iter()
            .filter(|(_, field)| !used_fields.contains_key(&field.name))
            .collect::<Vec<_>>();
...
        } else if !unmentioned_fields.is_empty() && !has_rest_pat {
            self.push_diagnostic(InferenceDiagnostic::RecordMissingFields {
                record: ExprOrPatId::PatId(pat),
                variant,
                missed_fields: unmentioned_fields.into_iter().map(|f| f.0).collect(),
            })
        }
```

After (the `let unmentioned_fields` is deleted; the list is made where it is read, only without
`..`, straight into the diagnostic's owned `Vec<LocalFieldId>`, which allocates nothing when
every field is mentioned; `used_fields` does not change after the loop, so it reads the same):

```rust
        } else if !has_rest_pat {
            let missed_fields: Vec<LocalFieldId> = variant_fields
                .fields()
                .iter()
                .filter(|(_, field)| !used_fields.contains_key(&field.name))
                .map(|(id, _)| id)
                .collect(); // alloc: the diagnostic's own list, only when fields are missing
            if !missed_fields.is_empty() {
                self.push_diagnostic(InferenceDiagnostic::RecordMissingFields {
                    record: ExprOrPatId::PatId(pat),
                    variant,
                    missed_fields,
                })
            }
        }
```

Imports: `collections::hash_map::Entry::{Occupied, Vacant},` leaves the `std::{..}` import
(3 to 7), which keeps `cmp, iter`; add `use hashbrown::hash_map::Entry::{Occupied, Vacant};`;
`use rustc_hash::FxHashMap;` (20, used only here) becomes `use rustc_hash::FxBuildHasher;`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): the tail had moved on to `if !matches!(variant, VariantId::UnionId(_)) && !unmentioned_fields.is_empty() && !has_rest_pat`; applied as `if !matches!(variant, VariantId::UnionId(_)) && !has_rest_pat { let missed_fields .. ; if !missed_fields.is_empty() { .. } }`, which pushes the same diagnostic in the same cases.

## callee.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E12 to E14 applied below (db-mem-infer-a-expr), in the tree.

### E12. Adjustments of the callee (lines 107 to 260)

Per call. Before (111): `autoderef: &mut InferenceContextAutoderef<'_, 'db>,`
After: `autoderef: &mut InferenceContextAutoderef<'_, 'db, 'f>,` (X3), and at the top of
`try_overloaded_call_step`'s body `let frame = autoderef.ctx().frame;`.

Before (120 to 123):

```rust
                let adjust_steps = autoderef.adjust_steps_as_infer_ok();
                let adjustments =
                    autoderef.ctx().table.register_infer_ok(adjust_steps).into_boxed_slice();
                autoderef.ctx().write_expr_adj(callee_expr, adjustments);
```

After:

```rust
                let adjust_steps = autoderef.adjust_steps_as_infer_ok(frame);
                let adjustments = autoderef.ctx().table.register_infer_ok(adjust_steps);
                autoderef.ctx().write_expr_adj(callee_expr, &adjustments);
```

The two `let adjust_steps = autoderef.adjust_steps_as_infer_ok();` of the deferred-closure arms
(139, 192) pass `frame`, and their `adjustments` go into `DeferredCallResolution` (E13). Before
(254 to 258):

```rust
                let adjustments = autoderef.adjust_steps_as_infer_ok();
                let mut adjustments = autoderef.ctx().table.register_infer_ok(adjustments);
                adjustments.extend(autoref);
                autoderef.ctx().write_expr_adj(callee_expr, adjustments.into_boxed_slice());
```

After:

```rust
                let adjustments = autoderef.adjust_steps_as_infer_ok(frame);
                let mut adjustments = autoderef.ctx().table.register_infer_ok(adjustments);
                adjustments.extend(autoref);
                autoderef.ctx().write_expr_adj(callee_expr, &adjustments);
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:22): as written, with E0's `callee.rs` header and `use blaze_mem::Frame;`.

### E13. `DeferredCallResolution` (lines 538 to 593)

Per call of a closure whose kind is not known yet; it lives in ctx's frame list (X4). Before:

```rust
#[derive(Debug, Clone)]
pub(crate) struct DeferredCallResolution<'db> {
    call_expr: ExprId,
    callee_expr: ExprId,
    closure_ty: Ty<'db>,
    adjustments: Vec<Adjustment>,
    fn_sig: FnSig<'db>,
}

impl<'db> DeferredCallResolution<'db> {
    pub(crate) fn resolve(self, ctx: &mut InferenceContext<'db>) {
```

After:

```rust
#[derive(Debug, Clone)]
pub(crate) struct DeferredCallResolution<'db, 'f> {
    call_expr: ExprId,
    callee_expr: ExprId,
    closure_ty: Ty<'db>,
    adjustments: Vec<Adjustment, &'f Frame>,
    fn_sig: FnSig<'db>,
}

impl<'db, 'f> DeferredCallResolution<'db, 'f> {
    pub(crate) fn resolve(self, ctx: &mut InferenceContext<'db, 'f>) {
```

and (584) `ctx.write_expr_adj(self.callee_expr, adjustments.into_boxed_slice());` becomes
`ctx.write_expr_adj(self.callee_expr, &adjustments);`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:22): as written.

### E14. `check_legacy_const_generics` (lines 358 to 426, 465)

Per call of a fn item. Before, every fn-item call computed `generics()` and collected its const
params before learning whether the function has legacy const generics (almost none do), then
copied the indices into a `Box`. After: the indices are copied into the frame, and
`generics()`/`const_params` are computed only past the early returns. `generics()` is a pure
read of the database, so computing it later changes nothing. Before:

```rust
    fn check_legacy_const_generics(
        &mut self,
        callee: Option<CallableDefId>,
        callee_ty: Ty<'db>,
        args: &[ExprId],
    ) -> Box<[u32]> {
        let (func, fn_generic_args) = match (callee, callee_ty.kind()) {
            (Some(CallableDefId::FunctionId(func)), TyKind::FnDef(_, fn_generic_args)) => {
                (func, fn_generic_args.no_bound_vars().unwrap())
            }
            _ => return Default::default(),
        };
        let generics = crate::generics::generics(self.db, func.into());
        let const_params = generics
            .iter_self_type_or_consts()
            .filter(|(_, param_data)| param_data.const_param().is_some())
            .map(|(id, _)| ConstParamId::from_unchecked(id))
            .collect::<Vec<_>>();

        let data = FunctionSignature::of(self.db, func);
        let Some(legacy_const_generics_indices) = data.legacy_const_generics_indices(self.db, func)
        else {
            return Default::default();
        };
        let mut legacy_const_generics_indices = Box::<[u32]>::from(legacy_const_generics_indices);

        // only use legacy const generics if the param count matches with them
        if data.params.len() + legacy_const_generics_indices.len() != args.len() {
            if args.len() <= data.params.len() {
                return Default::default();
            } else {
                // there are more parameters than there should be without legacy
                // const params; use them
                radix::sort(&mut legacy_const_generics_indices);
                return legacy_const_generics_indices;
            }
        }
```

After (the loop over the indices below is unchanged; it ends with
`radix::sort(legacy_const_generics_indices); legacy_const_generics_indices`, the `&mut` dropped
from the `radix::sort` call):

```rust
    fn check_legacy_const_generics(
        &mut self,
        callee: Option<CallableDefId>,
        callee_ty: Ty<'db>,
        args: &[ExprId],
    ) -> &'f mut [u32] {
        let frame = self.frame;
        let (func, fn_generic_args) = match (callee, callee_ty.kind()) {
            (Some(CallableDefId::FunctionId(func)), TyKind::FnDef(_, fn_generic_args)) => {
                (func, fn_generic_args.no_bound_vars().unwrap())
            }
            _ => return Default::default(),
        };
        let data = FunctionSignature::of(self.db, func);
        let Some(legacy_const_generics_indices) = data.legacy_const_generics_indices(self.db, func)
        else {
            return Default::default();
        };
        let legacy_const_generics_indices = frame.alloc_slice_copy(legacy_const_generics_indices);

        // only use legacy const generics if the param count matches with them
        if data.params.len() + legacy_const_generics_indices.len() != args.len() {
            if args.len() <= data.params.len() {
                return Default::default();
            } else {
                // there are more parameters than there should be without legacy
                // const params; use them
                radix::sort(legacy_const_generics_indices);
                return legacy_const_generics_indices;
            }
        }

        let generics = crate::generics::generics(self.db, func.into());
        let mut const_params = Vec::new_in(frame);
        const_params.extend(
            generics
                .iter_self_type_or_consts()
                .filter(|(_, param_data)| param_data.const_param().is_some())
                .map(|(id, _)| ConstParamId::from_unchecked(id)),
        );
```

The caller (465 to 471) `&indices_to_skip` stays (it derefs to `&[u32]`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:22): as written; both `radix::sort` calls drop the `&mut`.

## cast.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E15 applied below (db-mem-infer-a-expr), in the tree.

### E15. The auto-trait set of a dyn-to-dyn pointer cast (lines 388 to 405)

Per `*const dyn A as *const dyn B`. Before:

```rust
                        let src_auto: FxHashSet<_> = src_tty
                            .auto_traits()
                            .into_iter()
                            .chain(
                                elaborate::supertrait_def_ids(ctx.interner(), src_principal)
                                    .filter(|trait_| {
                                        TraitSignature::of(ctx.db, trait_.0)
                                            .flags
                                            .contains(TraitFlags::AUTO)
                                    }),
                            )
                            .collect();
```

After (a handful of auto traits: a linear `contains` over a frame list beats hashing):

```rust
                        let mut src_auto = Vec::new_in(ctx.frame);
                        src_auto.extend(src_tty.auto_traits().into_iter().chain(
                            elaborate::supertrait_def_ids(ctx.interner(), src_principal).filter(
                                |trait_| {
                                    TraitSignature::of(ctx.db, trait_.0)
                                        .flags
                                        .contains(TraitFlags::AUTO)
                                },
                            ),
                        ));
```

(`!src_auto.contains(&trait_)` after it keeps its text.) Remove `use rustc_hash::FxHashSet;`
(10, its only use). `CastCheck<'db>` itself is unchanged: 24 bytes, no heap; ctx keeps it in a
frame list (X4). Its methods take `&mut InferenceContext<'db, '_>` (E0).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:22): as written, with E0's eight `cast.rs` signatures (now lines 185 to 646); the `contains` reads `contains(trait_)` as it did.

## op.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E16 applied below (db-mem-infer-a-expr), in the tree.

### E16. The left operand's autoref (line 228)

Per overloaded binary operator by reference. Before:
`self.write_expr_adj(lhs_expr, Box::new([autoref]));`
After: `self.write_expr_adj(lhs_expr, &[autoref]);`
(the commented-out line 245 stays a comment.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:22): as written, with E0's `op.rs` header.

## place_op.rs

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): E17 applied below (db-mem-infer-a-expr), in the tree.

### E17. Overloaded deref and index adjustments (lines 64 to 70, 106 to 175)

Per overloaded `*x`. Before:

```rust
            self.write_expr_adj(
                oprnd_expr,
                Box::new([Adjustment {
                    kind: Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not)),
                    target: method.sig.inputs_and_output.inputs()[0].store(),
                }]),
            );
```

After:

```rust
            self.write_expr_adj(
                oprnd_expr,
                &[Adjustment {
                    kind: Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not)),
                    target: method.sig.inputs_and_output.inputs()[0].store(),
                }],
            );
```

Per overloaded `x[i]`: `try_index_step`'s `autoderef: &mut InferenceContextAutoderef<'_, 'db>,`
(110) becomes `autoderef: &mut InferenceContextAutoderef<'_, 'db, 'f>,` (X3). Before (154):
`let infer_ok = autoderef.adjust_steps_as_infer_ok();`, after:

```rust
                let frame = autoderef.ctx().frame;
                let infer_ok = autoderef.adjust_steps_as_infer_ok(frame);
```

(the two `adjustments.push(..)` that follow are unchanged), and (173)
`autoderef.ctx().write_expr_adj(base_expr, adjustments.into_boxed_slice());` becomes
`autoderef.ctx().write_expr_adj(base_expr, &adjustments);`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:22): as written, with E0's `place_op.rs` header.

## Tracked functions (R4, R6)

None in these seven files (`rg 'salsa::tracked'` finds nothing): they run inside `infer_query`
(and the closure and anon-const queries), whose memoized result and tag are ctx's and
mem-db-infer's. No memoized result is built here: everything above is a run temporary or a
value copied into ctx's tables.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): nothing to apply; still true (`rg 'salsa::tracked'` finds none in the six files).

## Entry points outside any query (R1)

- `coerce::could_coerce` (`pub`, re-exported at `infer.rs:127` and `lib.rs:98`): E5, its own
  `frame::task(Tag::Infer, ..)`. Callers: `crates/hir/src/lib.rs:6169` `Type::could_coerce_to`,
  from `crates/blaze-diagnostics/src/lower/{no_such_field.rs:533, mismatched_arg_count.rs:467,
  invalid_cast.rs:276, type_mismatch.rs:525, invalid_lhs_of_assignment.rs:88}`.
- Nothing else in these files is reachable from outside inference: `CastTy::from_ty` (`pub`) and
  `CastError` allocate nothing.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): `could_coerce` is E5, coerce.rs, db-mem-infer-a's; nothing in my six files is an entry point.

## Helpers on hit paths (R7)

None: these files run only when `infer_query` (or a closure or anon const in it) executes, which
is a miss. A memo hit or a deep verification that changes nothing never enters them.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): nothing to apply.

## Left as they are, with why

Every match of `.clone()`, `Vec::new()`, `vec![`, `.collect()`, `.to_vec()`, `Box::new(`,
`FxHashMap`, `FxHashSet`, `SmallVec`, `ThinVec`, `IndexSet`/`IndexMap` in the seven files after
the entries above:

- `coerce.rs:1082` `prev_adjustment.clone()`: an `Adjust`, a few bytes, no heap.
- `expr.rs:1831, 1839, 1917, 1953, 1974` `name.clone()` / `method_name.clone()`: `Name` is a
  `Copy` `u32` symbol (`crates/intern/src/symbol.rs:54`); the clone is a copy.
- `expr.rs:2110` `formal_input_tys.clone()` (now inside `FrameList(..)`): a frame clone, the
  value the fudge must hand back (the fudge rolls the snapshot back, so the list cannot be moved
  in and out).
- `expr.rs:2249` `skip_indices.clone()`: clones a `Copied<slice::Iter<u32>>`, two pointers.
- `pat.rs:1324` `InferOk { value: (), obligations: Vec::new() }`: `PredicateObligations`, the
  solver's type (mem-db-solver); empty unless a deref pattern binds `ref mut`, rare.
- `op.rs:316, 319` `Result<MethodCallee<'db>, Vec<NextSolverError<'db>>>` / `Err(vec![])`: the
  solver's error list on the failure path (empty `vec![]` allocates nothing), read by
  `unmet_goals`.
- `op.rs:391 to 403` `unmet_goals(..) -> Vec<StoredGoal>` with `.collect()`: the owned list of
  the `BinaryOperatorCannotBeApplied`/`UnaryOperatorCannotBeApplied` diagnostic, error path only.
- `pat.rs` (E11) `missed_fields` `.collect()`: the owned list of `RecordMissingFields`, error
  path only, marked `// alloc:` on its line.
- `coerce.rs` `PredicateObligations::with_capacity(2)` (321) and the `InferOk` obligations
  threaded through every `CoerceResult`: the solver's `Vec<PredicateObligation>`
  (`next_solver/infer/traits.rs:203`), mem-db-solver's to move; the coercion code only passes it
  along and `extend`s it.
- `callee.rs:310` `Ty::new_tup_from_iter`, `pat.rs:1128` `Tys::new_from_iter`, `pat.rs:1144`
  `Ty::new_tup_from_iter`, `cast.rs:353, 365` `BoundExistentialPredicates::new_from_iter`:
  interned values (`Tag::Intern`), not temporaries.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): left as listed. Also left, pre-existing on error paths: `expr.rs` `call_arguments`' `provided`/`inputs_tys`/`compatible` lists and `Box<CallArguments>` (db-ty-d-infer1's, marked `// alloc:`), `cast.rs` `added_auto_traits` (`Box::new([])` allocates nothing; `added.collect()` is the failure's own list).

## Tests

Kept (they hold the semantics of every path changed here; run as they are):

- `crates/hir-ide/src/tests/coercion.rs`: coercion sites, LUBs and their recorded adjustments
  (`// ^^^^ adjustments: ..` annotations, e.g. `coerce_fn_items_in_match_arms` for E3/E4,
  `coerce_places` for E1/E2), never-to-any, unsizing.
- `crates/hir-ide/src/tests/patterns.rs`: record, tuple, slice, ref and box patterns (E11).
- `crates/hir-ide/src/tests/simple.rs`: struct literals (E8), tuples (E7), legacy const generics
  (E14).
- `crates/hir-ide/src/tests/regression.rs:2981`
  `regression_unresolved_deferred_closure_call_resolution` (E13), `method_resolution.rs` (E9,
  E12), `consteval.rs:525` `overloaded_index` (E17), `never_type.rs` (E1, E4).
- `crates/blaze-diagnostics/src/tests/bodies/members.rs`: duplicate, missing and non-existent
  fields of literals and patterns (E8, E11), rustc's codes.
- The diagnostics tests that go through `could_coerce` (`blaze-diagnostics`' type mismatch,
  invalid cast and no-such-field lowering) hold E5's answer.

New, for `crates/hir-ide/src/tests/coercion.rs` (the owner of `crates/hir-ide` applies them).
They pin the paths whose sizes changed: a `match` past the old four inline slots, so every
earlier arm's adjustment is rewritten from the frame list (E3, E4); and a call whose expected
input types come back through the in-place fudge (E10), on the plain and the tupled path:

```rust
#[test]
fn coerce_fn_items_in_match_with_many_arms() {
    check_no_mismatches(
        r"
fn foo1(x: u32) -> isize { 1 }
fn foo2(x: u32) -> isize { 2 }
fn foo3(x: u32) -> isize { 3 }
fn foo4(x: u32) -> isize { 4 }
fn foo5(x: u32) -> isize { 5 }
fn foo6(x: u32) -> isize { 6 }
fn test() {
    let x = match 1 {
        1 => foo1,
          // ^^^^ adjustments: Pointer(ReifyFnPointer)
        2 => foo2,
          // ^^^^ adjustments: Pointer(ReifyFnPointer)
        3 => foo3,
          // ^^^^ adjustments: Pointer(ReifyFnPointer)
        4 => foo4,
          // ^^^^ adjustments: Pointer(ReifyFnPointer)
        5 => foo5,
          // ^^^^ adjustments: Pointer(ReifyFnPointer)
        _ => foo6,
          // ^^^^ adjustments: Pointer(ReifyFnPointer)
    };
    x;
}",
    );
    check_types(
        r"
fn foo1(x: u32) -> isize { 1 }
fn foo2(x: u32) -> isize { 2 }
fn foo3(x: u32) -> isize { 3 }
fn foo4(x: u32) -> isize { 4 }
fn foo5(x: u32) -> isize { 5 }
fn foo6(x: u32) -> isize { 6 }
fn test() {
    let x = match 1 {
        1 => foo1,
        2 => foo2,
        3 => foo3,
        4 => foo4,
        5 => foo5,
        _ => foo6,
    };
    x;
  //^ fn(u32) -> isize
}",
    );
}

#[test]
fn call_argument_expectations_from_expected_output() {
    check_types(
        r"
//- minicore: fn
struct Wrap<T>(T);
fn wrap<T>(t: T) -> Wrap<T> { Wrap(t) }
fn test() {
    let w: Wrap<&[u8]> = wrap(&[1, 2, 3]);
    let f = |a, b| (a, b);
    let p: (u8, u16) = f(1, 2);
    p.0;
  //^^^ u8
    p.1;
  //^^^ u16
}",
    );
}
```

(In `wrap(&[1, 2, 3])` the expected output `Wrap<&[u8]>` gives the argument the expectation
`&[u8]`, which coerces the array reference by unsizing, as rustc does. The closure call takes
the deferred, tupled path of E10 and E13.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): the two new tests filed in `v2/requests/db.md` for the coordinator (owner of `crates/hir-ide`).

## Speed

The bound for these files' temporaries, on a 1,000-expression body (about 250 calls of two
arguments, 300 coercions that record adjustments, 60 struct literals and 30 record patterns of
three fields, 20 `match`es of four to six arms, 30 tuples): their bytes written once to warm
memory, plus a frame allocation (about 2 ns) each.

| temporary | count | bytes each | bytes per run |
|---|---|---|---|
| call input lists (formal, expected, tupled) | 250 × 1 to 3 | 16 | 8 KB |
| adjustment lists (coercion, autoderef) | 300 × 1 to 2 | 8 to 24 | 6 KB, mostly freed in place as the frame's top block |
| struct-literal maps | 60 × 2 | about 90 (4 buckets of 16 bytes + control bytes) | 11 KB |
| record-pattern maps and lists | 30 × 3 | about 90 | 8 KB |
| `CoerceMany` lists | 20 `match` + 100 blocks/loops | 4 per arm | under 1 KB |
| tuple element lists | 30 | 16 | under 1 KB |
| deferred calls, casts | a few | 24 to 64 | under 1 KB |

About 35 KB and 1,500 frame allocations per run: about 3 µs of allocation and 1 to 4 µs of
stores (L1/L2-resident), against the same body's inference of 1 to 5 ms (the solver
dominates). Before: about 1,800 global allocations and frees (about 2.5 per call, 1 to 2 per
recorded coercion, 3 to 5 per struct literal or record pattern, 1 per big `match`) at 15 to 25 ns
each, 30 to 45 µs, plus the re-collecting folds' copies. These files' temporaries are thus about
0.1% of the run. Each is made once at its final size where the size is known (formal lists,
tuple lists, field maps, `match` arm lists, autoderef lists). Where it isn't (adjustment lists
of 1 to 3, return and break lists), the frame grows the top block in place. That is the bytes
written once, within 10% of the bound. The remaining global allocations on these paths are the
solver's (`PredicateObligations`, `InferCtxt`, `goal.candidates()`) and autoderef's `steps`,
which belong to mem-db-solver and mem-db-infer-method.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-expr (21:33): nothing to apply in code; the six files' temporaries are now as this section counts them.
