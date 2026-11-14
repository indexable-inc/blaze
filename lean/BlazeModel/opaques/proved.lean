-- Hand-written, not generated: the proofs of the `opaques` fixture's statements.
import Blaze.Auto
import BlazeModel.opaques.pass_keeps_ok
import BlazeModel.opaques.failed_keeps_its_code

/-!
The first proofs over opaque items (crates/hir-ty-lean/src/tests.rs, `OPAQUES`;
docs/v2/plan/design/proofs-for-sans-io.md, section 3, step 0). `std::io::Error` is
`core.io.error.Error`, declared in `Blaze.Opaque` with no definition, so each statement holds for
every meaning of it: the proofs carry its values and never look inside one.

- `pass_keeps_ok`: `pass` gives `Ok(x)` back as `Ok(x)` and any `Err(e)` back as an error.
- `failed_keeps_its_code`: the code of `fail(code, why)` is `code`, whatever `why` is.
-/

open Blaze Std.WP Lean.Order

theorem opaques.pass_keeps_ok.spec.proof : opaques.pass_keeps_ok.spec.statement := by
  intro x e
  blaze_auto [opaques.pass_keeps_ok, opaques.pass, core.result.Result.is_err]

theorem opaques.failed_keeps_its_code.spec.proof :
    opaques.failed_keeps_its_code.spec.statement := by
  intro code why
  blaze_auto [opaques.failed_keeps_its_code, opaques.fail, opaques.code_of]
