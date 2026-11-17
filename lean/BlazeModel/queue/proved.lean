-- Hand-written, not generated: the proofs of the `queue` fixture's three statements.
import Blaze.VecAuto
import Blaze.Machine
import BlazeModel.queue.new_is_wf
import BlazeModel.queue.offer_keeps_wf
import BlazeModel.queue.take_keeps_wf

/-!
The first state machine over a `Vec` proved (crates/hir-ty-lean/src/tests.rs, `QUEUE`;
docs/v2/plan/design/proofs-for-sans-io.md, section 3 step d). A queue is well formed when it
holds at most `limit` items; a new queue is, and `offer` and `take` keep it so, without
panicking. As every proof about a `Vec`, these trust that each allocation succeeds
(`Blaze.Vec`).
-/

open Blaze Std.WP Lean.Order

theorem queue.new_is_wf.spec.proof : queue.new_is_wf.spec.statement := by
  intro limit
  blaze_auto [queue.new_is_wf, queue.Queue.new, queue.Queue.wf]

theorem queue.offer_keeps_wf.spec.proof : queue.offer_keeps_wf.spec.statement := by
  intro q item
  blaze_auto [queue.offer_keeps_wf, queue.Queue.wf, queue.Queue.offer]

theorem queue.take_keeps_wf.spec.proof : queue.take_keeps_wf.spec.statement := by
  intro q
  blaze_auto [queue.take_keeps_wf, queue.Queue.wf, queue.Queue.take]

namespace queue.Queue.machine

open Blaze.Machine

/-- One ask: `some item` offers `item`, `none` takes the newest item. -/
def step (q : queue.Queue) : Option U32 → Result queue.Queue
  | some item => ofMethod queue.Queue.offer q item
  | none => Result.ok (queue.Queue.take q).2

/-- A queue is well formed when its own `wf` says so. -/
def wf (q : queue.Queue) : Prop := queue.Queue.wf q = true

/-- Obligation 1, from `new_is_wf`: a new queue is well formed. -/
theorem wf_new (limit : Usize) : wf (queue.Queue.new limit) := queue.new_is_wf.spec.proof limit

/-- Obligation 2, from `offer_keeps_wf` and `take_keeps_wf`: from a well-formed queue each ask
succeeds and keeps it well formed. -/
theorem step_ok (q : queue.Queue) (a : Option U32) (h : wf q) (_ : True) :
    ∃ q', step q a = Result.ok q' ∧ wf q' := by
  cases a with
  | none =>
    have hk := queue.take_keeps_wf.spec.proof q
    simp only [queue.take_keeps_wf, wf] at hk h
    rcases ht : queue.Queue.take q with ⟨t, q'⟩
    simp only [h, ht, Bool.not_true] at hk
    exact ⟨q', by simp only [step, ht], hk⟩
  | some item =>
    have hk := queue.offer_keeps_wf.spec.proof q item
    simp only [queue.offer_keeps_wf, wf] at hk h
    rcases ho : queue.Queue.offer q item with ⟨b, q'⟩ | e | _
    · simp only [h, ho, Bool.not_true, Result.bind_ok] at hk
      exact ⟨q', by simp only [step, ofMethod, ho, Result.bind_ok], by injection hk⟩
    · simp only [h, ho, Bool.not_true, Result.bind_fail] at hk
      cases hk
    · simp only [h, ho, Bool.not_true, Result.bind_div] at hk
      cases hk

/-- `items.len() ≤ limit` after every prefix of any run of offers and takes of a new queue,
and no step fails. -/
example (limit : Usize) (asks pre : List (Option U32)) (hpre : pre <+: asks) :
    ∃ q, run step (queue.Queue.new limit) pre = Result.ok q ∧
      (q.items.val.length : Int) ≤ q.limit.val :=
  always_fn (init := fun q => q = queue.Queue.new limit) (allowed := fun _ _ => True)
    (fun _ h => h ▸ wf_new limit) step_ok
    (fun q h => by simpa [wf, queue.Queue.wf] using h)
    _ _ _ rfl (Allowed.of_forall (fun _ _ => trivial) _ _) hpre

end queue.Queue.machine
