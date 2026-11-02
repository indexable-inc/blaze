# Requests from db-ty-d-infer1

## To diagnostics (`crates/blaze-diagnostics/src/lower/invalid_lhs_of_assignment.rs`)

`hir::InvalidLhsOfAssignment` now carries `assign: InFile<ExprOrPatPtr>` (the `=` or `op=`
expression) and `op: Option<ast::ArithOp>` (`None` for `=`, E0070; `Some` for E0067). Replace
the `lhs_syntax.ancestors().filter_map(ast::BinExpr::cast)` search with `cx.node(d.assign)`
(its `left()` is the `ast::BinExpr`; take `op_token()` from it), and `compound` with
`d.op.is_some()`. A plain `=` with a non-place left-hand side is now reported too (it was not
before), so `1 = 2;` reaches this lowering.

The same change for `crates/ide-diagnostics/src/handlers/invalid_lhs_of_assignment.rs`: the code
is `if d.op.is_some() { "E0067" } else { "E0070" }` rather than always `E0067`.

> forwarded to diagnostics (told by tmux).

## To diagnostics and ide-diagnostics: two new `AnyDiagnostic` variants (E0695, E0727)

`hir::UnlabeledJumpInLabeledBlock { expr: InFile<ExprOrPatPtr>, is_break: bool }` and
`hir::AsyncCoroutinesNotSupported { expr: InFile<ExprOrPatPtr> }` are new. Both
`crates/blaze-diagnostics/src/lower/mod.rs` and `crates/ide-diagnostics/src/lib.rs` match
`AnyDiagnostic` exhaustively, so each needs an arm:

- `UnlabeledJumpInLabeledBlock`: rustc's `UnlabeledInLabeledBlock`. In
  `lower/break_outside_of_loop.rs` the E0695 branch of `diagnostic` (the
  `stack.last() == Some(&Context::LabeledBlock)` case) moves to a `lower_unlabeled(cx, d)` for
  this variant: `E0695 "unlabeled `{name}` inside of a labeled block"` on the jump, labelled
  "`{name}` statements that would diverge to or through a labeled block need to bear a label".
  hir-ty now gives this instead of `BreakOutsideOfLoop` for such a jump, inside a loop or not
  (rustc's `require_label_in_labeled_block` returns before any other check), so the old branch
  is never reached; a `try` block's desugared label does not count, as in rustc.
  Tests: `tests/ui/label/label_break_value_unlabeled_break.rs`, `label_break_value_continue.rs`.
- `AsyncCoroutinesNotSupported`: `E0727 "`async` coroutines are not yet supported"` on the
  `yield` expression (rustc's `lower_expr_yield`). The `yield`'s operand is still type checked
  and the `yield` is the error type. Test: `tests/ui/coroutine/async-coroutine-issue-67158.rs`.
  ide-diagnostics: `DiagnosticCode::RustcHardError("E0695")`/`("E0727")` with the same messages.

> forwarded to diagnostics (blaze-diagnostics and ide-diagnostics are its; told by tmux).

## To diagnostics (`crates/blaze-diagnostics/src/lower/{binary,unary}_operator_cannot_be_applied.rs`)

`hir::BinaryOperatorCannotBeApplied` now carries `unmet: Vec<SolverGoal>` and
`output: Option<Type>`, and `hir::UnaryOperatorCannotBeApplied` carries `unmet`. `unmet` is
rustc's `errors` of the failed lookup: each fulfillment error's leaf goal (`X: PartialEq` for
`W(X) == W(X)` with `impl<T: PartialEq> PartialEq for W<T>`; the operator's own
`lhs: Trait<rhs>` when nothing nested failed), empty when the operator's trait is missing.
`output` is `ObligationCauseCode::BinOp`'s `output_ty` (the expected type, as resolved at the
end of inference). With them the lowering can port `note_unmet_impls_on_type` (the trait
goals on ADT self types: "an implementation of `..` might be missing for `..`", the derive
suggestion when `can_eq(lhs, rhs)`), the `suggest_restricting_param_bound` loop when `lhs`
has a type parameter (with `Output = {output}` for a binop whose output is suggestable), and
"the trait `..` is not implemented for `..`" exactly when `unmet` is empty, in place of
knowing only the operator's own trait. Tests: `binop/issue-93927.rs`,
`suggestions/invalid-bin-op.rs`, `binop/binary-op-not-allowed-issue-125631.rs`.

> forwarded to diagnostics (from requests, pass 16; told by tmux).

## To diagnostics (`lower/{binary,unary}_operator_cannot_be_applied.rs`, `cannot_be_dereferenced.rs`, `cannot_index_into.rs`)

`hir::BinaryOperatorCannotBeApplied.at_error: Option<(String, String)>` and
`hir::{UnaryOperatorCannotBeApplied, CannotBeDereferenced, CannotIndexInto}.at_error:
Option<String>` are the operand types as rustc prints them when it reports the error, before
integer fallback (`{integer}`/`{float}` for an undecided literal; `true + 1` is "cannot add
`{integer}` to `bool`", `*1` is "type `{integer}` cannot be dereferenced"). They are `None`
when no operand held an undecided literal, which is when the `Type` fields already print the
same. Each message and label printing those types takes `at_error` when it is `Some`. Tests:
`binop/binop-typeck.rs`, `error-codes/E0614.rs`, `error-codes/E0600.rs`.

> forwarded to diagnostics (from requests, pass 17; told by tmux).

## To diagnostics and ide-diagnostics: `MismatchedArgCount.args`, new `IncorrectArguments`

`hir::MismatchedArgCount` now carries `args: hir::CallArgs`, and the new
`hir::IncorrectArguments { call_expr: InFile<ExprOrPatPtr>, args: CallArgs }` is a call whose
argument count fits but some argument does not (rustc's `report_arg_errors` reached with
`call_appears_satisfied` false: E0308 "arguments to this function are incorrect"). Each such
argument also has its own `TypeMismatch`, so ide-diagnostics may leave it silent, but both `crates/blaze-diagnostics/src/lower/mod.rs` and
`crates/ide-diagnostics/src/lib.rs` match `AnyDiagnostic` exhaustively and need an arm.

`CallArgs` is what `report_arg_errors` starts from, every type a `String` as rustc prints it at
the error: `callee` (rustc's `callee_ty`, the receiver's type of a method call), `method`
(`Option<Function>`), `is_method`, `c_variadic`, `formal_inputs`, `expected_inputs`, `provided`
(the arguments' adjusted types; of a C-variadic call only those of fixed inputs), and
`compatible_with(i)`, `ArgMatrix::new`'s row of the `i`th provided argument (the inputs it
fits, the diagonal as `check_argument_types` found it and the rest by `check_compatible`).
With it the lowering can port `ArgMatrix::find_issue`'s loop (missing, extra, swapped,
permuted, invalid arguments) and its labels and suggestions, for both E0061/E0057 and
E0308, in place of knowing only the counts. Tests: `argument-suggestions/extra_arguments.rs`,
`missing_arguments.rs`, `swapped_arguments.rs`, `permuted_arguments.rs`,
`invalid_arguments.rs`, `error-codes/E0057.rs`.

> forwarded to diagnostics (from requests, pass 17; told by tmux).
