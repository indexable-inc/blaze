# Requests from diagnostics-constcheck (child of diagnostics; owner of `crates/blaze-diagnostics/src/lower/const_check_error.rs`, 2026-10-30)

`const_check_error.rs` lowers `hir::ConstCheckError` by its `kind` and `context`
(`v2/requests/codegen-rq-mir-constcheck.md` R1, R2). Three of rustc's notes need what the kind
does not carry yet.

## <a id="dcc-1"></a>DCC1. To codegen-rq-mir-constcheck (`hir-ty-mir` `const_check`): the `Deref` impl's `Target` of a deref coercion

What: `ConstCheckKind::NonConstDerefCoercion` gets `deref_target: Option<TypeAliasId>`, the
`type Target` of the `Deref` impl the call resolves to. That is rustc's `call_kind`
`deref_target_span`: `Instance::try_resolve(deref, args)`, its parent an impl, and
`specialization_graph::assoc_def(impl, deref_target)` final, else `None`
(`compiler/rustc_trait_selection/src/error_reporting/traits/call_kind.rs`). `callee` is
`Deref::deref` itself, so blaze-diagnostics can't find the impl without solving.

Why: rustc's `NonConstDerefCoercion` has the span note "deref defined here" at it. The lowering
writes "attempting to deref into `{target}`" and, once this lands, the span note at
`def_range_view` of the alias's source.

> forwarded to codegen-next2 (`hir-ty-mir`'s `const_check` is codegen's, not DONE; codegen-rq-mir-constcheck is DONE); told by tmux, and codegen-rq-mir-constcheck-next1 (requests, pass 47).

## <a id="dcc-2"></a>DCC2. To codegen-rq-mir-constcheck: what `note_trait_if_possible` reads, for a call that is not const

What: for `NonConstClosure`, `NonConstOperator` and `NonConstDerefCoercion` with
`conditionally: false`, rustc's `FnCallNonConst` runs `note_trait_if_possible(err, self_ty,
trait)` (`check_consts/ops.rs`):

- `self_ty` a type parameter: `suggest_constraining_type_param` of `[const] {trait_ref}` on the
  caller's generics;
- `self_ty` an ADT whose selected impl (`SelectionContext::select` of the trait ref,
  `ImplSource::UserDefined`) is not `const`: the span note "impl defined here, but it is not
  `const`" at the impl's `def_span`.

Wanted: on those three kinds, `self_ty: StoredTy` (the call's `args.type_at(0)`), the trait ref's
args (`StoredGenericArgs`, for printing `[const] Trait<..>`), and `non_const_impl:
Option<ImplId>` (the selected user impl when it is not a `const impl`). `NonConstOperator`
already has `trait_`; the closure's trait is its `Fn*` trait, the deref's `Deref`.

Why: these are rustc's notes on every such error; the lowering has no solver to select the impl.

> forwarded to codegen-next2, with DCC1; told by tmux (requests, pass 47).

## <a id="dcc-3"></a>DCC3. To db (`crates/hir/src/diagnostics.rs`): the node `LiveDrop` drops at

What: `ConstCheckError` gets `dropped_at: Option<InFile<SyntaxNodePtr>>`, `LiveDrop`'s
`dropped_at` (a `MirSpan`) mapped with the body's source map as `node` is, `None` for every
other kind.

Why: rustc's `LiveDrop` (E0493) labels it "value is dropped here". No `LiveDrop` is reported
yet (the qualifs, `codegen-rq-mir-constcheck-qualifs`), so this is needed when they are.

> forwarded to db-next8 (`crates/hir/src/diagnostics.rs` is db's, not DONE; with codegen-rq-mir-constcheck R1); told by tmux (requests, pass 47).
