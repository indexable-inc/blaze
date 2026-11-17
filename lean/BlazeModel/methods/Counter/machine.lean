-- Hand-written, not generated: `Blaze.Machine` applied to the generated `methods.Counter`.
import Blaze.Machine
import BlazeModel.methods.Counter.type
import BlazeModel.methods.Counter.new
import BlazeModel.methods.Counter.bump

/-!
The generated `methods.Counter` as a machine: the state is the counter, an ask is one call of
`bump`, and `hits ≤ limit` holds after every bump of `methods.Counter.new limit`.
-/

open Blaze Blaze.Machine

namespace methods.Counter.machine

/-- One ask is one call of `bump`; the step keeps the new counter. -/
def step : methods.Counter → Unit → Result methods.Counter :=
  ofMethod fun c (_ : Unit) => methods.Counter.bump c

/-- The counter is well formed when it has not passed its limit. -/
def wf (c : methods.Counter) : Prop := c.hits.val ≤ c.limit.val

/-- Obligation 1: a new counter is well formed. -/
theorem wf_new (limit : U32) : wf (methods.Counter.new limit) := by
  simp only [wf, methods.Counter.new, Scalar.ofNat_val .U32 0 (by decide)]
  exact limit.hmin

/-- Obligation 2: from a well-formed counter a bump succeeds and keeps it well formed. -/
theorem step_ok (c : methods.Counter) (_ : Unit) (h : wf c) (_ : True) :
    ∃ c', step c () = Result.ok c' ∧ wf c' := by
  simp only [step, ofMethod, methods.Counter.bump]
  by_cases hlt : c.hits.val < c.limit.val
  · have h1 : (1 : U32).val = 1 := Scalar.ofNat_val .U32 1 (by decide)
    have hmin := c.hits.hmin
    have hmax := c.limit.hmax
    rw [ite_eq_left ((Scalar.lt_iff _ _).2 hlt),
      Scalar.add_ok _ _ (by simp only [h1, ScalarTy.min, ScalarTy.max] at *; omega)]
    exact ⟨_, rfl, by simp only [wf, h1]; omega⟩
  · rw [ite_eq_right (by rw [Scalar.lt_iff]; exact hlt)]
    exact ⟨_, rfl, h⟩

/-- `hits ≤ limit` after every prefix of any number of bumps of a new counter, and no bump
fails. -/
example (limit : U32) (n : Nat) (pre : List Unit) (hpre : pre <+: List.replicate n ()) :
    ∃ c, run step (methods.Counter.new limit) pre = Result.ok c ∧ c.hits.val ≤ c.limit.val :=
  always_fn (init := fun c => c = methods.Counter.new limit) (allowed := fun _ _ => True)
    (fun _ h => h ▸ wf_new limit) step_ok (fun _ h => h)
    _ _ _ rfl (Allowed.of_forall (fun _ _ => trivial) _ _) hpre

/-- The same through the relational corollary: every state any run of bumps may reach has
`hits ≤ limit`. -/
example (limit : U32) (n : Nat) (c : methods.Counter)
    (h : Run (ofFn step) (methods.Counter.new limit) (List.replicate n ()) c) :
    c.hits.val ≤ c.limit.val :=
  have obl := ofFn_obligations (allowed := fun _ _ => True) step_ok
  (always (init := fun c => c = methods.Counter.new limit) (fun _ h => h ▸ wf_new limit)
    obl.1 obl.2 (fun _ h => h) _ _ _ rfl (Allowed.of_forall (fun _ _ => trivial) _ _)
    List.prefix_rfl).2 c h

end methods.Counter.machine
