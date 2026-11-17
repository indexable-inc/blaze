-- Hand-written, not generated: what the `fmts` fixture's models of a derived `Debug` give.
import Blaze
import BlazeModel.fmts.show_point

/-!
A derived `Debug` is modelled over opaque builders (crates/hir-ty-lean/src/tests.rs, `FMTS`), on
the assumption of formatting that lean/Blaze/Opaque.lean lists under what is trusted: the output
of a `fmt` is a function of the value formatted and the formatter's state.

A builder is used linearly: after each call of it the formatter is
`core.fmt.builders.DebugStruct.formatter` of the builder. A naive model, which took `field` and
`finish` as functions of the builder that leave the formatter alone (the formatter after `fmt`
being the one `debug_struct` gave back), would prove by `rfl`

    ∀ f, (Prod.snd <$> fmts.show_point ⟨1, 2⟩ f) = (Prod.snd <$> fmts.show_point ⟨1, 3⟩ f)

which is false of Rust: `Point { x: 1, y: 2 }` and `Point { x: 1, y: 3 }` write different bytes
to the formatter. Under the linear model the formatter after is `formatter` of the builder after
`finish`, which the `&dyn Debug` of `y` reaches, so neither `rfl` nor `decide` proves it: the
first example below checks that, with the pinned Lean, on every build. What does hold is the
weaker statement after it: points with equal fields are written alike.
-/

open Blaze

/-- The statement a naive model would prove is not proved by `rfl` or `decide` under this one;
the control first, so the check cannot pass by the statement failing to elaborate: the same
statement of one point is proved by `rfl`. -/
example : True := by
  have _control : ∀ f, (Prod.snd <$> fmts.show_point ⟨1, 2⟩ f) = (Prod.snd <$> fmts.show_point ⟨1, 2⟩ f) := by
    intro f; rfl
  fail_if_success
    (have : ∀ f, (Prod.snd <$> fmts.show_point ⟨1, 2⟩ f) = (Prod.snd <$> fmts.show_point ⟨1, 3⟩ f) := by
      intro f; rfl)
  fail_if_success
    (have : ∀ f, (Prod.snd <$> fmts.show_point ⟨1, 2⟩ f) = (Prod.snd <$> fmts.show_point ⟨1, 3⟩ f) := by
      decide)
  trivial

/-- Formatting is a function of the value: two points with equal fields write the same result
and leave the formatter the same. -/
example (p q : fmts.Point) (f : core.fmt.Formatter) (hx : p.x = q.x) (hy : p.y = q.y) :
    fmts.show_point p f = fmts.show_point q f := by
  cases p; cases q
  simp only at hx hy
  subst hx; subst hy
  rfl
