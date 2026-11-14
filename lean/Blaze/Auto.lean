import Blaze
import Std.WP
import Std.Tactic.Do
import Lean.Elab.Tactic

open Std.WP Lean.Order

namespace Blaze

/-- The postcondition shape of a `Result` beyond its value: what a panic must satisfy (given its
`Error`), and what running out of fuel must satisfy. -/
abbrev Result.EPost := EStack⟨Error → Prop, Unit → Prop⟩

/-- `Result` is a lawful monad: its `bind` is `Option`'s with two ways to stop. -/
instance Result.instLawfulMonad : LawfulMonad Result :=
  LawfulMonad.mk' Result
    (id_map := fun x => by cases x <;> rfl)
    (pure_bind := fun _ _ => rfl)
    (bind_assoc := fun x _ _ => by cases x <;> rfl)

/-- The weakest precondition of a `Result`: `post v` of a value, the first exception
postcondition of a panic, the second of running out of fuel. -/
instance Result.wpInst {α : Type} : WP (Result α) α Prop Result.EPost where
  wpTrans x := ⟨fun post epost => match x with
    | .ok v => post v
    | .fail e => epost.1 e
    | .div => epost.2.1 ()⟩
  wp_trans_monotone x := fun _ _ _ _ hepost hpost => by
    cases x with
    | ok v => exact hpost v
    | fail e => exact hepost.1 e
    | div => exact hepost.2.1 ()

/-- `Result`'s weakest precondition commutes with `pure` and `bind`. -/
instance Result.instWPMonad : WPMonad Result Prop Result.EPost where
  toWP _ := inferInstance
  pure_le_wp_pure _ _ _ := PartialOrder.rel_refl
  bind_le_wp_bind x _ := fun _ _ => by cases x <;> exact id

/-- With no exception postcondition given, a panic satisfies nothing. -/
@[simp, grind =] theorem Result.bot_fail (e : Error) : (⊥ : Result.EPost).1 e = False := by
  rw [Prod.fst_bot, bot_apply, bot_prop_eq]

/-- With no exception postcondition given, running out of fuel satisfies nothing. -/
@[simp, grind =] theorem Result.bot_div (u : Unit) : (⊥ : Result.EPost).2.1 u = False := by
  rw [Prod.snd_bot, Prod.fst_bot, bot_apply, bot_prop_eq]

/-- A value satisfies `post` of itself. -/
@[spec] theorem Result.ok_spec {α : Type} (v : α) {post : α → Prop} {epost : Result.EPost} :
    Triple (Result.ok v) (post v) post epost := ⟨PartialOrder.rel_refl⟩

/-- A panic satisfies the panic's postcondition. -/
@[spec] theorem Result.fail_spec {α : Type} (e : Error) {post : α → Prop} {epost : Result.EPost} :
    Triple (Result.fail e : Result α) (epost.1 e) post epost := ⟨PartialOrder.rel_refl⟩

/-- Running out of fuel satisfies the second exception postcondition. -/
@[spec] theorem Result.div_spec {α : Type} {post : α → Prop} {epost : Result.EPost} :
    Triple (Result.div : Result α) (epost.2.1 ()) post epost := ⟨PartialOrder.rel_refl⟩

/-- Partial correctness: from a triple whose panic postcondition is `False` and whose
out-of-fuel postcondition is `True`, any `P x` follows that holds of every value satisfying `Q`
and of running out of fuel. With `P` the generated statement's `match`, both side conditions hold
by computation. -/
theorem Result.partial_cases {α : Type} {x : Result α} {Q : α → Prop} (P : Result α → Prop)
    (hok : ∀ r, Q r → P (.ok r)) (hdiv : P .div)
    (h : Triple x True Q estack⟨fun _ => False, fun _ => True⟩) : P x := by
  have hw := h.le_wp trivial
  cases x with
  | ok r => exact hok r hw
  | fail _ => exact hw.elim
  | div => exact hdiv

/-- Total correctness: a triple with no exception postcondition gives a value that `Q` holds of. -/
theorem Result.ok_of_triple {α : Type} {x : Result α} {Q : α → Prop}
    (h : Triple x True Q ⊥) : ∃ r, x = .ok r ∧ Q r := by
  have hw := h.le_wp trivial
  cases x with
  | ok v => exact ⟨v, rfl, hw⟩
  | fail e => exact absurd hw (by simp [WP.wp, WP.wpTrans])
  | div => exact absurd hw (by simp [WP.wp, WP.wpTrans])

/-- Total correctness as an equation: the computation gives `v`. -/
theorem Result.eq_ok_of_triple {α : Type} {x : Result α} {v : α}
    (h : Triple x True (fun r => r = v) ⊥) : x = .ok v := by
  obtain ⟨r, hx, rfl⟩ := Result.ok_of_triple h
  exact hx

/-- Total correctness without a property: the computation gives a value. -/
theorem Result.exists_ok_of_triple {α : Type} {x : Result α}
    (h : Triple x True (fun _ => True) ⊥) : ∃ r, x = .ok r :=
  (Result.ok_of_triple h).imp fun _ h => h.1

/-! Specifications of the operations the models call: when each succeeds and what it gives.
A goal about a panic (that an operation fails) is proved by `simp` with the operation's own
definition instead. -/

namespace Scalar

variable {ty : ScalarTy} {epost : Result.EPost}

/-- `checked ty x` succeeds with `x` when `x` is in range. -/
theorem checked_spec (x : Int) :
    Triple (checked ty x) (ty.min ≤ x ∧ x ≤ ty.max) (fun r => r.val = x) epost := by
  constructor
  intro h
  unfold checked
  rw [dite_eq_left h]
  rfl

/-- `a + b` succeeds with the sum when it is in range. -/
@[spec] theorem add_spec (a b : Scalar ty) :
    Triple (add a b) (ty.min ≤ a.val + b.val ∧ a.val + b.val ≤ ty.max)
      (fun r => r.val = a.val + b.val) epost :=
  checked_spec _

/-- `a - b` succeeds with the difference when it is in range. -/
@[spec] theorem sub_spec (a b : Scalar ty) :
    Triple (sub a b) (ty.min ≤ a.val - b.val ∧ a.val - b.val ≤ ty.max)
      (fun r => r.val = a.val - b.val) epost :=
  checked_spec _

/-- `a * b` succeeds with the product when it is in range. -/
@[spec] theorem mul_spec (a b : Scalar ty) :
    Triple (mul a b) (ty.min ≤ a.val * b.val ∧ a.val * b.val ≤ ty.max)
      (fun r => r.val = a.val * b.val) epost :=
  checked_spec _

/-- `a / b` succeeds with the quotient toward zero when `b` is not zero and the quotient is in
range (it is not, for `MIN / -1`). -/
@[spec] theorem div_spec (a b : Scalar ty) :
    Triple (div a b)
      (b.val ≠ 0 ∧ ty.min ≤ a.val.tdiv b.val ∧ a.val.tdiv b.val ≤ ty.max)
      (fun r => r.val = a.val.tdiv b.val) epost := by
  constructor
  intro h
  unfold div
  rw [ite_eq_right h.1]
  exact (checked_spec (epost := epost) _).le_wp h.2

/-- `a % b` succeeds with the remainder (the sign of `a`) when `b` is not zero and not
`MIN % -1`. -/
@[spec] theorem rem_spec (a b : Scalar ty) :
    Triple (rem a b)
      (¬ (b.val = 0 ∨ (a.val = ty.min ∧ b.val = -1)) ∧
        ty.min ≤ a.val.tmod b.val ∧ a.val.tmod b.val ≤ ty.max)
      (fun r => r.val = a.val.tmod b.val) epost := by
  constructor
  intro h
  unfold rem
  rw [ite_eq_right h.1]
  exact (checked_spec (epost := epost) _).le_wp h.2

/-- `a < b` is the comparison of the values. -/
@[simp, grind =] theorem lt_eq (a b : Scalar ty) : (lt a b = true) = (a.val < b.val) := by
  simp [lt]
/-- `a <= b` is the comparison of the values. -/
@[simp, grind =] theorem le_eq (a b : Scalar ty) : (le a b = true) = (a.val ≤ b.val) := by
  simp [le]
/-- `a > b` is the comparison of the values. -/
@[simp, grind =] theorem gt_eq (a b : Scalar ty) : (gt a b = true) = (b.val < a.val) := by
  simp [gt]
/-- `a >= b` is the comparison of the values. -/
@[simp, grind =] theorem ge_eq (a b : Scalar ty) : (ge a b = true) = (b.val ≤ a.val) := by
  simp [ge]

/-- `==` on machine integers is equality of their values. -/
@[simp, grind =] theorem eq_true_eq (a b : Scalar ty) : (eq a b = true) = (a.val = b.val) := by
  simp [eq]

/-- `!=` on machine integers is inequality of their values. -/
@[simp, grind =] theorem ne_true_eq (a b : Scalar ty) : (ne a b = true) = (a.val ≠ b.val) := by
  simp [ne]

/-- Every integer is at least its type's minimum; `grind` learns it from each value it sees. -/
theorem min_le_val (x : Scalar ty) : ty.min ≤ x.val := x.hmin
/-- Every integer is at most its type's maximum. -/
theorem val_le_max (x : Scalar ty) : x.val ≤ ty.max := x.hmax
grind_pattern min_le_val => x.val
grind_pattern val_le_max => x.val
/-- Two integers of a type are equal when their values are. -/
@[grind inj] theorem val_injective : Function.Injective (val (ty := ty)) := fun _ _ => ext

end Scalar

/-- A quotient of naturals is a natural at most the dividend. `grind` rewrites `tdiv` to `/`
and its linear arithmetic knows nothing of `/` by a variable, so it learns this from each `/`. -/
theorem ediv_bounds (a b : Int) (ha : 0 ≤ a) (hb : 0 ≤ b) : 0 ≤ a / b ∧ a / b ≤ a :=
  ⟨Int.ediv_nonneg ha hb, Int.ediv_le_self b ha⟩
grind_pattern ediv_bounds => a / b

/-- A remainder by a positive divisor is in `[0, b)`; `grind` learns it from each `%`. -/
theorem emod_bounds (a b : Int) (hb : 0 < b) : 0 ≤ a % b ∧ a % b < b :=
  ⟨Int.emod_nonneg a (by omega), Int.emod_lt_of_pos a hb⟩
grind_pattern emod_bounds => a % b

/-- `s[i]` succeeds with the element at `i` when `i` is in bounds. -/
@[spec] theorem Slice.index_spec {α : Type} (s : Slice α) (i : Usize) {epost : Result.EPost} :
    Triple (Slice.index s i) (i.val.toNat < s.val.length)
      (fun r => s.val[i.val.toNat]? = some r) epost := by
  constructor
  intro h
  unfold Slice.index
  rw [dite_eq_left h]
  exact List.getElem?_eq_getElem h

attribute [simp, grind =] Slice.len_val

/-- A slice has at most `usize::MAX` elements; `grind` learns it from each length it sees. -/
theorem Slice.length_le {α : Type} (s : Slice α) : s.val.length ≤ 18446744073709551615 := s.hlen
grind_pattern Slice.length_le => s.val.length

/-- `assert!(b)` succeeds when `b` holds. -/
@[spec] theorem massert_spec (b : Bool) {epost : Result.EPost} :
    Triple (massert b) (b = true) (fun _ => True) epost := by
  constructor
  intro h
  rw [h]
  trivial

/-- `blaze_vcs [defs]`: on a Hoare triple, `vcgen [defs]`; on any other goal, unfold `defs`.
`vcgen`'s own failure is the tactic's failure: nothing else is tried. `vcgen` is marked
experimental in Lean 4.35.0 and warns at each call unless `experimental.vcgen` acknowledges it;
it is set here, the one place blaze calls it (`mvcgen`, the stable one, is deprecated for it). -/
syntax "blaze_vcs" " [" Lean.Parser.Tactic.simpLemma,* "]" : tactic

/-- Every goal is closed; otherwise an error names each goal left open. -/
syntax "blaze_all_closed" : tactic

/-- A goal that is a generated statement's `match` on a `Result` (value, panic, out of fuel)
becomes the partial-correctness triple of `Result.partial_cases`. The discriminant and the
value's alternative are read off the `match`: unification does not see through a `match` on a
call that does not compute. -/
syntax "blaze_partial" : tactic

open Lean Elab Tactic Meta in
elab_rules : tactic
  | `(tactic| blaze_vcs [$ls,*]) => do
    if (← whnfR (← getMainTarget)).isAppOf ``Std.WP.Triple then
      evalTactic (← `(tactic| set_option experimental.vcgen true in vcgen [$ls,*]))
    else
      evalTactic (← `(tactic| try simp only [$ls,*] at *))
  | `(tactic| blaze_partial) => do
    let g ← getMainGoal
    let tgt ← instantiateMVars (← g.getType)
    let some app ← matchMatcherApp? tgt
      | throwError "blaze_auto: the goal is not a `match`"
    unless app.discrs.size == 1 && app.alts.size == 3 do
      throwError "blaze_auto: the `match` is not over one `Result` with three alternatives"
    let x := app.discrs[0]!
    let motive := mkLambda `res .default (← inferType x) (← kabstract tgt x)
    let pf ← mkAppOptM ``Blaze.Result.partial_cases #[none, x, app.alts[0]!, motive]
    let [gok, gdiv, gt] ← g.apply pf
      | throwError "blaze_auto: `Result.partial_cases` did not leave three goals"
    setGoals [gok]
    evalTactic (← `(tactic| (intro _ h; exact h)))
    setGoals [gdiv]
    evalTactic (← `(tactic| trivial))
    setGoals [gt]
  | `(tactic| blaze_all_closed) => do
    let gs ← getGoals
    unless gs.isEmpty do
      let shown := MessageData.joinSep (gs.map MessageData.ofGoal) m!"\n\n"
      throwError m!"blaze_auto: {gs.length} goal(s) left open:{indentD shown}"

/-- `blaze_auto [defs]` proves a statement about a model: a Hoare triple, the generated
partial-correctness `match`, or total correctness (`f args = .ok v`, `∃ r, f args = .ok r ∧ Q r`,
or `∃ r, f args = .ok r`). It turns the statement into a triple, generates its verification
conditions with `vcgen` (unfolding `defs`, using the `@[spec]`s and any triple given in `defs`,
such as an induction hypothesis or a loop's invariant lemma), unfolds `defs` in what is left, and
closes each goal with `grind` over the integers' bounds (and `ediv_bounds`, `emod_bounds`). A statement that is not about a `Result`
(a total function, or that an operation fails) is unfolded and given to `grind` directly. It
fails, naming each goal it left open, when it cannot close them all. -/
syntax "blaze_auto" (" [" term,* "]")? : tactic

macro_rules
  | `(tactic| blaze_auto) => `(tactic| blaze_auto [])
  | `(tactic| blaze_auto [$ts,*]) => do
    let ls ← ts.getElems.mapM fun t => `(Lean.Parser.Tactic.simpLemma| $t:term)
    `(tactic| (
    intros
    first
      | blaze_partial
      | apply Blaze.Result.eq_ok_of_triple
      | apply Blaze.Result.ok_of_triple
      | apply Blaze.Result.exists_ok_of_triple
      | skip
    blaze_vcs [$ls,*]
    all_goals try simp only [$ls,*] at *
    all_goals try
      grind [ScalarTy.min, ScalarTy.max, Scalar.ofNat_val, Scalar.add, Scalar.sub, Scalar.mul,
        Scalar.checked, List.length_pos_iff, List.head?_eq_getElem?]
    blaze_all_closed))

end Blaze
