-- Hand-written, not generated: the proofs of the `defaults` fixture's statements.
import Blaze
import BlazeModel.defaults.derived_default_is_each_fields

/-!
`Default::default` proved through its models (crates/hir-ty-lean/src/tests.rs, `DEFAULTS`): a
derived `Default` of a struct is each field's default (an integer's zero, `false`, `None`, the
empty `Vec`, a tuple of defaults, and the hand-written `Clock::default`, whose `ticks` is 7), a
derived `Default` of an enum is its `#[default]` variant, and an array's default is its element
type's default in each place. A model that gave any field another value fails this.
-/

open Blaze

theorem defaults.derived_default_is_each_fields.spec.proof :
    defaults.derived_default_is_each_fields.spec.statement := by
  rfl
