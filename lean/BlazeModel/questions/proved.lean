-- Hand-written, not generated: the proofs of the `questions` fixture's statements.
import Blaze.Auto
import BlazeModel.questions.sum_two_adds_or_gives_the_first_err
import BlazeModel.questions.sum_opts_adds_or_is_none
import BlazeModel.questions.widen_keeps_ok_and_converts_err

/-!
The first proofs over `?` (crates/hir-ty-lean/src/tests.rs, `QUESTIONS`;
docs/v2/plan/design/proofs-for-sans-io.md, section 3, step b). `x?` is core's own `Try::branch`
at its instance, a match on the `ControlFlow` it gives, and core's own
`FromResidual::from_residual` returned in its `Break` arm, its error converted by the `From` impl
at the instance.

- `sum_two_adds_or_gives_the_first_err`: `Ok(a? + b?)` of two `Ok`s is their sum when it does
  not overflow, and of an `Err` the first `Err`.
- `sum_opts_adds_or_is_none`: the same of `Option`: the sum of two `Some`s, else `None`.
- `widen_keeps_ok_and_converts_err`: `?` from `Result<u32, Small>` into `Result<u32, Big>` keeps an
  `Ok` and converts an `Err` with the crate's own `From<Small> for Big`.
-/

open Blaze Std.WP Lean.Order

/-- A cast of an integer to its own type is the integer: what `From<u8> for u8`, the error
conversion of `?` between two `Result`s with one error type, is in the model. -/
theorem questions.cast_self {ty : ScalarTy} (a : Scalar ty) : Scalar.cast ty a = a :=
  Scalar.ext (Scalar.cast_val ty a ⟨a.hmin, a.hmax⟩)

theorem questions.sum_two_adds_or_gives_the_first_err.spec.proof :
    questions.sum_two_adds_or_gives_the_first_err.spec.statement := by
  intro a b e f
  blaze_auto [questions.sum_two_adds_or_gives_the_first_err, questions.sum_two,
    core.result.Result.u32_u8.Try.branch,
    core.result.Result.u32_u8.FromResidual.Result_never_u8.from_residual, questions.cast_self]

theorem questions.sum_opts_adds_or_is_none.spec.proof :
    questions.sum_opts_adds_or_is_none.spec.statement := by
  intro a b
  blaze_auto [questions.sum_opts_adds_or_is_none, questions.sum_opts,
    core.option.Option.u32.Try.branch,
    core.option.Option.u32.FromResidual.Option_never.from_residual]

theorem questions.widen_keeps_ok_and_converts_err.spec.proof :
    questions.widen_keeps_ok_and_converts_err.spec.statement := by
  intro x v
  blaze_auto [questions.widen_keeps_ok_and_converts_err, questions.widen,
    core.result.Result.u32_Small.Try.branch,
    core.result.Result.u32_Big.FromResidual.Result_never_Small.from_residual,
    questions.Big.From.Small.«from»]
