import Blaze.Auto
import BlazeModel.arith.add_one
import BlazeModel.early.clamp
import BlazeModel.methods.Counter.bump
import BlazeModel.methods.twice
import BlazeModel.loops.count
import BlazeModel.blaze_graph.ledger.fnv1a_from
import BlazeModel.slices.first

open Blaze Std.WP Lean.Order

example (x : U32) (h : x.val < 4294967295) :
    ∃ r, arith.add_one x = Result.ok r ∧ r.val = x.val + 1 := by
  blaze_auto [arith.add_one]

example (x : U32) (h : ¬ x.val < 4294967295) :
    arith.add_one x = Result.fail .arithmetic := by
  blaze_auto [arith.add_one]

example (x limit : U32) : (early.clamp x limit).val ≤ limit.val := by
  blaze_auto [early.clamp]

/-- A counter is well formed when its hits are at most its limit. -/
def wf (c : methods.Counter) : Prop := c.hits.val ≤ c.limit.val

example (c : methods.Counter) (h : wf c) :
    ∃ r, methods.Counter.bump c = Result.ok r ∧ wf r.2 := by
  blaze_auto [methods.Counter.bump, wf]

example (limit : U32) : ∃ r, methods.twice limit = Result.ok r := by
  blaze_auto [methods.twice, methods.Counter.bump, methods.Counter.new, methods.Counter.hits_fn]

/-- The invariant of `count`'s loop, stated by hand: from `i ≤ n` with more fuel than turns left,
it returns `n`. -/
theorem count_loop (n : U32) : ∀ (fuel : Nat) (i : U32),
    Triple (loops.count.loop_0 fuel n i) (i.val ≤ n.val ∧ n.val - i.val < fuel)
      (fun r => r = n) ⊥ := by
  intro fuel
  induction fuel with
  | zero => blaze_auto [loops.count.loop_0]
  | succ k ih => blaze_auto [loops.count.loop_0, ih]

example (n : U32) : ∃ fuel, loops.count fuel n = Result.ok n := by
  refine ⟨n.val.toNat + 1, ?_⟩
  blaze_auto [loops.count, count_loop]

/-- The invariant of `fnv1a_from`'s loop, stated by hand: `at` stays within the bytes, and with no
bytes the hash is still the seed. -/
theorem fnv_loop (hash0 : U64) (bytes : Slice U8) : ∀ (fuel : Nat) (h : U64) (i : Usize),
    Triple (blaze_graph.ledger.fnv1a_from.loop_0 fuel h bytes i)
      (i.val ≤ (Slice.len bytes).val ∧ ((Slice.len bytes).val = 0 → h.val = hash0.val))
      (fun r => (Slice.len bytes).val ≠ 0 ∨ r.val = hash0.val)
      estack⟨fun _ => False, fun _ => True⟩ := by
  intro fuel
  induction fuel with
  | zero => blaze_auto [blaze_graph.ledger.fnv1a_from.loop_0]
  | succ k ih => blaze_auto [blaze_graph.ledger.fnv1a_from.loop_0, ih]

example : blaze_graph.ledger.fnv1a_from.spec.statement := by
  intro fuel hash bytes
  blaze_auto [blaze_graph.ledger.fnv1a_from, fnv_loop hash bytes]

example (xs : Slice U32) (h : xs.val ≠ []) :
    ∃ r, slices.first xs = Result.ok r ∧ xs.val.head? = some r := by
  blaze_auto [slices.first]

example (a b : U32) (h : b.val ≤ a.val) :
    ∃ r, Scalar.sub a b = Result.ok r ∧ r.val + b.val = a.val := by
  blaze_auto

example (a : U32) (h : a.val < 1000) :
    ∃ r, Scalar.mul a 4 = Result.ok r ∧ r.val = 4 * a.val := by
  blaze_auto

example (a b : U32) (h : b.val ≠ 0) :
    ∃ r, Scalar.div a b = Result.ok r ∧ r.val ≤ a.val := by
  blaze_auto

example (a b : I32) (h : 0 < b.val) :
    ∃ r, Scalar.rem a b = Result.ok r ∧ r.val < b.val := by
  blaze_auto

example (a b : U32) : ∃ r, massert (Scalar.le a b || Scalar.ge a b) = Result.ok r := by
  blaze_auto

/--
error: blaze_auto: 1 goal(s) left open:
  case vc1
  x : Scalar ScalarTy.U32
  ⊢ ScalarTy.U32.min ≤ x.val + Scalar.val 1 ∧ x.val + Scalar.val 1 ≤ ScalarTy.U32.max
-/
#guard_msgs in
example (x : U32) : ∃ r, arith.add_one x = Result.ok r := by
  blaze_auto [arith.add_one]
