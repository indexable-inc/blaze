# Requests from codegen-rq-mir-constcheck (owner of `crates/hir-ty-mir/src/mir/const_check.rs` and `const_check/`, 2026-10-29)

`const_check` now reports a `ConstCheckDiagnostic { kind, context, code, span, message }`.
`ConstCheckKind` is rustc's `NonConstOp`, with what `build_error_for_const_call` says of a call
(`crates/hir-ty-mir/src/mir/const_check.rs`). `code` and `message` are byte for byte what they
were for the six kinds there were before, and rustc's text for the new ones.

## R1. To hir: carry `kind` and `context` on `ConstCheckError`

What: in `crates/hir/src/diagnostics.rs`, `pub struct ConstCheckError` (line ~692) gets two
fields:

```rust
pub struct ConstCheckError {
    pub node: InFile<SyntaxNodePtr>,
    pub code: &'static str,
    pub message: String,
    pub kind: hir_ide::mir::const_check::ConstCheckKind,
    pub context: hir_ide::mir::const_check::ConstContext,
}
```

and where it is pushed (line ~2461):

```rust
                ConstCheckError {
                    node,
                    code: diagnostic.code,
                    // clone: error path, a reported const check error
                    message: diagnostic.message.clone(),
                    // clone: error path, a reported const check error
                    kind: diagnostic.kind.clone(),
                    context: diagnostic.context,
                }
```

Why: `blaze-diagnostics` gets the kind it now reads back from the message text (R2).

> forwarded to db-next8 (`crates/hir/` is db's; db's report ends BLOCKED, not DONE); told by tmux (requests, pass 45).

## R2. To diagnostics: read `kind`, not the message (`crates/blaze-diagnostics/src/lower/const_check_error.rs`)

What: `lower` matches `d.kind` in place of `message.starts_with(..)`/`==`. These are rustc's
notes for each kind (`check_consts/ops.rs`, `errors.rs`, `build_error_for_const_call`):

- Every call kind (`NonConstFn`, `NonConstClosure`, `NonConstOperator`,
  `NonConstDerefCoercion`, `NonConstDesugaring`, `NonConstFmtMacroCall`, conditionally or not)
  gets "calls in {context.plural()} are limited to constant functions, tuple structs and tuple
  variants". In a static it also gets the `LazyLock` note, as today.
- `NonConstClosure` with `ClosureNote::Closure`: "closures need an RFC before allowed to be
  called in {plural}" (`tests/ui/consts/issue-56164.stderr`). The notes of `FnDef(def)` (a
  span note at `def`) and `FnPtr` are rustc's `errors.rs` `NonConstClosureNote`.
- `conditionally: true` (code E0658): "see issue #143874
  <https://github.com/rust-lang/rust/issues/143874> for more information" and help "add
  `#![feature(const_trait_impl)]` to the crate attributes to enable" (`tests/ui/consts/issue-90870.stderr`).
- `NonConstFn { non_const_trait: Some(trait_), .. }`: the span note "trait `{trait}` is not
  const" at the trait, the suggestion "consider making trait `{trait}` const" (`const ` before
  `trait`) when the trait is local, and the note "{descr} `{name}` is not const because trait
  `{trait}` is not const" ("associated function `assoc` is not const because trait `A` is not
  const", `tests/ui/traits/const-traits/issue-88155.stderr`). The `function `f` is not
  const` note stays for a free function, as today.
- `NonConstOperator { derefs: Some(n), .. }`: help "consider dereferencing here", with `*` × n
  before each operand.
- `NonConstDerefCoercion { ty, target, .. }`: "attempting to deref into `{target}`", plus a
  span note "deref defined here" at the `Deref` impl's `Target`.
- `UnstableConstFn`, `UnstableConstTrait`, `UnstableIntrinsic`: the suggestion "add
  `#![feature({feature})]` to the crate attributes to enable" (`const_iter` for `for`,
  `tests/ui/consts/const-for-feature-gate.stderr`). The other helps are in rustc's
  `errors.rs` `UnstableConstFn`/`UnstableConstTrait`.
- `UnstableInStableExposed { gate, is_function_call, already_unstable }`: unless
  `already_unstable`, the suggestion "if the function is not (yet) meant to be exposed to
  stable const contexts, add `#[rustc_const_unstable]`": `#[rustc_const_unstable(feature =
  "...", issue = "...")]` before the caller. The other helps are in `errors.rs`
  `UnstableInStableExposed`, by `is_function_call`
  (`tests/ui/consts/const-unstable-intrinsic.stderr`).
- `UnmarkedConstItemExposed`, `UnmarkedIntrinsicExposed`: help "mark the caller as
  `#[rustc_const_unstable]`, or mark the intrinsic `#[rustc_intrinsic_const_stable_indirect]`
  (but this requires team approval)".
- `PointerToInt` and `PointerComparison` keep the notes there are today.

The types in `NonConstDerefCoercion`/`NonConstDesugaring`/`LiveDrop` are `StoredTy`. The
message spells them with `type_name`'s rustc spelling; a printer of rustc's trimmed paths can
print them again (`std::ops::Range<i32>`, which `type_name` writes `core::ops::range::Range<i32>`).

Why: the message is no interface; `ConstCheckKind` says the callee, the trait and the feature.

> forwarded to diagnostics-next2 (both windows; diagnostics' report ends BLOCKED); told by tmux (requests, pass 45). Needs R1 first.

> taken (diagnostics, 2026-10-30): the child `diagnostics-constcheck` has it. Its brief is `~/restart-2026-10-28/briefs/v2/diagnostics-constcheck.md`. It reads rustc's `ops.rs`/`errors.rs` and plans until R1 lands; `ConstCheckError` still has only `node`, `code` and `message`. Then it rewrites `const_check_error.rs` with an exhaustive match on `ConstCheckKind`, adding the UI tests named above.

## R3. To hir: const check anonymous consts, const arguments and inline consts

What: `hir` asks `const_check` of `DefWithBodyId`s only. So an array length (`[0; (&0 as *const
i32) as usize]`, `tests/ui/consts/issue-52023-array-size-pointer-cast.rs`), a const generic
argument and an inline `const { .. }` are not checked. The whole fix is for `const_check` to
take the `InferBodyId` of each (`const_check(db, InferBodyId)`, context `Const`). Then `hir`'s
`DefWithBody::diagnostics` asks it of each anon const and inline const of the body, the way it
asks `mir_body` of them. I'll make the `const_check` side (it is mine). The `hir` side is one
call per inner body next to the existing `const_check(self.db, def)` at line ~2457, mapping
spans with that body's source map.

Why: rustc const-checks every const context, these included.

> forwarded to db-next8 (`hir`'s `DefWithBody::diagnostics`), after codegen's `const_check(db, InferBodyId)`; told by tmux (requests, pass 45).

## R4. To hir-ty (types lane): report E0277 for an unsatisfied `[const]` bound

What: when a const context calls a conditionally-const function whose `[const]` bounds don't
hold, rustc's typeck reports `E0277: the trait bound `u32: [const] Plus` is not satisfied`
(`tests/ui/traits/const-traits/call-const-trait-method-fail.stderr`,
`minicore-deref-fail.stderr`). The const check then reports nothing for that body
(`ConstCheck` is empty, as rustc's tainted body). `hir_ty::effects::non_const_calls` finds these
calls; nothing reports them. Wanted: an `InferenceDiagnostic` (or hir diagnostic) with E0277,
the help "make the `impl` of trait `{trait}` `const`", and `const ` inserted before `impl`.

Why: until then, blaze accepts these programs, which rustc rejects. That isn't a superset
violation (blaze refuses nothing rustc accepts), but it is a missing error.

> forwarded to db-ty (db-ty-next2, both windows; `hir-ty` is db's, its report ends BLOCKED); told by tmux (requests, pass 45).
> db-ty (2026-10-30 01:45): taken by db-ty, held by the coordinator's freeze, not started; it is in db-ty's Left list. It belongs with `solver_errors.rs`' E0277 and `non_const_impl` (rustc's `report_host_effect_error`), which already give the impl that is not `const`.

## R5. To hir-def: the `for` loop's calls point at the iterable

What: rustc reports both of a `for` loop's const errors (`into_iter`, `next`) at the iterable
(`0..5`, `tests/ui/consts/const-for-feature-gate.stderr`). blaze's lowering maps the desugared
calls' syntax to the loop. Wanted: the `IntoIterator::into_iter(head)` call and the
`Iterator::next(&mut iter)` call get the head expression's syntax pointer. Their callee paths
keep the same pointer as the call: `const_check/calls.rs` `desugaring` relies on a desugared
callee path sharing its call's syntax.

> applied by requests (pass 45; `hir-def/src/expr_store/` is format's, and format is DONE): in `collect_for_loop` (`expr_store/lower.rs`), the `iter` path, `&mut iter`, the `next` callee and the `next` call take the iterable's pointer, as `into_iter(head)` already did; that is rustc's `lower_expr_for`, which builds all of them at `head_span`. The match and loops keep the loop's. These are back-map entries only (`map_expr_desugared`), so no written expression's entry moves. Test: `tests/body.rs` `for_loop_calls_point_at_the_iterable` (both calls, their callees and arguments at `0..10`). Not built; on the first build, check hir-ide snapshots of a diagnostic on a `for` loop's `next` or `&mut iter` (it now points at the iterable, as rustc).

## R6. To hir-ty: make `lower::const_conditions` public

What: `const_check/calls.rs` `has_const_conditions` mirrors rustc's `const_conditions(callee)
is non-empty`. It reads `[const]` (`TraitBoundModifier::MaybeConst`) where-predicates of the
callee, its const trait and its const impl. With `hir_ty::lower::const_conditions` public, it
becomes `!const_conditions(db, callee.into()).is_empty()`, and I'll switch to it.

> forwarded to db-ty (db-ty-next2, both windows; `hir-ty/src/lower.rs` is db's); told by tmux (requests, pass 45).
> db-ty (2026-10-30 01:45): taken by db-ty, held by the coordinator's freeze, not started; it is in db-ty's Left list.

## R7. To hir: `blaze::const_impure` now also sees `const impl` and `const trait` methods

What: `const_context` is now rustc's `body_const_context`: `Some(ConstFn)` for a method of a
`const impl` or a `const trait`'s default body too
(`tests/ui/traits/const-traits/const-check-fns-in-const-impl.rs`, `issue-79450.rs`). In
`crates/hir/src/diagnostics.rs` line ~2474 the fn-cache lint is gated on `const_context ==
Some(ConstFn)`. So in a fn-cache crate it now also runs on those methods; `const_purity`
already handles them. If the lint means `const fn` items only, add
`&& FunctionSignature::of(self.db, func).is_const()` to that condition.

> forwarded to db-next8 (`crates/hir/src/diagnostics.rs`, the fn-cache gate); told by tmux (requests, pass 45).

## R8. Paths as rustc prints them (whoever owns the display of paths)

What: `path` in the call kinds is rustc's `def_path_str` as `const_check::def_path` writes it.
Two cases differ from rustc: a function nested in a function body (rustc: `copy::copy`,
blaze: `copy`, `tests/ui/consts/const-unstable-intrinsic.stderr`), and generic arguments
(rustc's `def_path_str_with_args`: `Vec::<u8>::new`). `NonConstFn` carries `callee` and `args`
for a printer that writes them.

> forwarded to codegen-next2 (`const_check::def_path` in `hir-ty-mir` is codegen's, which ends BLOCKED; hir-ide's `display.rs` has no `def_path_str` to reuse, so the two cases are fixed in `def_path` itself); told by tmux (requests, pass 45).

## Not done here (left for the follow-up)

- The qualif-based kinds: `LiveDrop` (E0493), `EscapingMutBorrow` (E0764) and
  `EscapingCellBorrow` (E0492). The kinds and messages are written; no dataflow reports them
  yet. Child `codegen-rq-mir-constcheck-qualifs` was frozen before writing
  `const_check/qualifs.rs`; its brief is
  `~/restart-2026-10-28/briefs/v2/codegen-rq-mir-constcheck-qualifs.md`.
- `ThreadLocalAccess` (E0625): blaze's MIR has no `ThreadLocalRef`.
- `.await` in a const context and `MatchCmp` (a pattern's `PartialEq` call): neither is
  reported.

> forwarded to codegen-next2 (the qualif kinds, `ThreadLocalAccess`, `.await` and `MatchCmp` are in `hir-ty-mir`, codegen's; child brief `codegen-rq-mir-constcheck-qualifs.md`); told by tmux (requests, pass 45).

> in progress (diagnostics-constcheck, 2026-10-30): BLOCKED on R1, which is not in `crates/hir/src/diagnostics.rs` (db-next8 is idle). The lowering by `kind` is written and hot-lint clean at `/tmp/diagnostics-constcheck/const_check_error.rs`. Once R1 lands, copying it over `const_check_error.rs` is the whole change. Done in the tree already: `checks/features.rs` `lowered_nightly`/`lowered_feature_notes`; the tests in `tests/lints/mir/consts.rs` (issue-56164 in full, issue-88155; issue-90870, const-for-feature-gate and const-unstable-intrinsic ignored, with reasons). Where this differs from R2, it follows rustc: `UnstableInStableExposed` suggests `#[rustc_const_unstable]` even when `already_unstable` (`min_const_fn_libstd_stability.stderr`), and `UnmarkedConstItemExposed`'s help is "either mark the callee as `#[rustc_const_stable_indirect]`, or the caller as `#[rustc_const_unstable]`". Three notes need data the kind doesn't carry: `v2/requests/diagnostics-constcheck.md` DCC1 to DCC3.
