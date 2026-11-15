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

/-- `a << b` succeeds when the shift is less than the width; with the product in range no bit
is lost and it is `a * 2 ^ b`. (A shift that drops bits is `wrap` of the product: unfold `shl`
for it.) -/
@[spec] theorem shl_spec {ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty') :
    Triple (shl a b)
      ((0 ≤ b.val ∧ b.val < (ty.bits : Int)) ∧
        ty.min ≤ a.val * 2 ^ b.val.toNat ∧ a.val * 2 ^ b.val.toNat ≤ ty.max)
      (fun r => r.val = a.val * 2 ^ b.val.toNat) epost := by
  constructor
  intro h
  unfold shl
  rw [ite_eq_left h.1]
  exact wrap_val ty _ h.2

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

/-! A comparison that is `false`, as arithmetic: what a branch not taken, and the condition of a
Rust `&&` or `||` once `simp` has opened it, say. -/

/-- `a < b` is false when `b ≤ a`. -/
@[simp, grind =] theorem lt_eq_false (a b : Scalar ty) : (lt a b = false) = (b.val ≤ a.val) := by
  simp [lt]
/-- `a <= b` is false when `b < a`. -/
@[simp, grind =] theorem le_eq_false (a b : Scalar ty) : (le a b = false) = (b.val < a.val) := by
  simp [le]
/-- `a > b` is false when `a ≤ b`. -/
@[simp, grind =] theorem gt_eq_false (a b : Scalar ty) : (gt a b = false) = (a.val ≤ b.val) := by
  simp [gt]
/-- `a >= b` is false when `a < b`. -/
@[simp, grind =] theorem ge_eq_false (a b : Scalar ty) : (ge a b = false) = (a.val < b.val) := by
  simp [ge]
/-- `a == b` is false when the values differ. -/
@[simp, grind =] theorem eq_false_eq (a b : Scalar ty) : (eq a b = false) = (a.val ≠ b.val) := by
  simp [eq]
/-- `a != b` is false when the values are equal. -/
@[simp, grind =] theorem ne_false_eq (a b : Scalar ty) : (ne a b = false) = (a.val = b.val) := by
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

/-- A power of two is at least one; `grind` learns it from each one it sees, so a shift's
`a * 2 ^ n` is bounded below. -/
theorem one_le_two_pow (n : Nat) : (1 : Int) ≤ 2 ^ n := by
  have h : (0 : Int) < 2 ^ n := Int.pow_pos (by decide)
  omega
grind_pattern one_le_two_pow => (2 : Int) ^ n

/-- Powers of two grow with the exponent: the bound a shift by a bounded amount needs
(`two_pow_le n 20`, with `n ≤ 20`, bounds `2 ^ n` by `2 ^ 20`). Given as a hypothesis where it
is needed: `grind` cannot guess the larger exponent. -/
theorem two_pow_le (n m : Nat) (h : n ≤ m) : (2 : Int) ^ n ≤ 2 ^ m := by
  have h' : ((2 ^ n : Nat) : Int) ≤ ((2 ^ m : Nat) : Int) :=
    Int.ofNat_le.mpr (Nat.pow_le_pow_right (by decide) h)
  rwa [Int.natCast_pow, Int.natCast_pow] at h'

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

/-- A call no case reaches satisfies anything: the specification `blaze_auto` gives each opaque
function (`opaque f : .. → Result R`, no definition) for `vcgen`. Its precondition is `False`, so
a call that some case does reach leaves `False` to prove under that case's path condition, and the
proof fails there, as it must: nothing is assumed of what an opaque function gives. -/
theorem Result.false_spec {α : Type} (x : Result α) {post : α → Prop} {epost : Result.EPost} :
    Triple x False post epost := by
  constructor
  intro h
  exact h.elim

/-- `blaze_auto_core [defs]`: the pipeline `blaze_auto` runs on one case once its arguments are
split. It turns the goal into a triple, generates its verification conditions with `vcgen`
(unfolding `defs`, using the `@[spec]`s and any triple given in `defs`), unfolds `defs` in what
is left, opens what Rust's `&&`, `||` and `!` became into propositions about the integers'
values, and closes each goal with `grind`; it fails naming each goal it left open. -/
syntax "blaze_auto_core" " [" term,* "]" : tactic

macro_rules
  | `(tactic| blaze_auto_core [$ts,*]) => do
    let ls ← ts.getElems.mapM fun t => `(Lean.Parser.Tactic.simpLemma| $t:term)
    `(tactic| (
    first
      | blaze_partial
      | apply Blaze.Result.eq_ok_of_triple
      | apply Blaze.Result.ok_of_triple
      | apply Blaze.Result.exists_ok_of_triple
      | skip
    blaze_vcs [$ls,*]
    all_goals try simp only [$ls,*] at *
    all_goals try (set_option linter.unusedSimpArgs false in
      simp only [Bool.not_eq_true', Bool.not_eq_false', Bool.not_eq_true, Bool.not_eq_false,
        Bool.ite_eq_true_distrib, Bool.ite_eq_false_distrib, Scalar.lt_eq, Scalar.le_eq,
        Scalar.gt_eq, Scalar.ge_eq, Scalar.eq_true_eq, Scalar.ne_true_eq, Scalar.lt_eq_false,
        Scalar.le_eq_false, Scalar.gt_eq_false, Scalar.ge_eq_false, Scalar.eq_false_eq,
        Scalar.ne_false_eq, $ls,*] at *)
    all_goals try
      grind [ScalarTy.min, ScalarTy.max, ScalarTy.bits, Scalar.ofNat_val, Scalar.add, Scalar.sub, Scalar.mul,
        Scalar.checked, List.length_pos_iff, List.head?_eq_getElem?]
    blaze_all_closed))

/-- A cast of an integer to its own type is the integer (`From<u8> for u8`, the error conversion
of `?` between two `Result`s with one error type). -/
theorem Scalar.cast_self {ty : ScalarTy} (a : Scalar ty) : Scalar.cast ty a = a :=
  Scalar.ext (Scalar.cast_val ty a ⟨a.hmin, a.hmax⟩)

/-- A cast to a type at least as wide keeps the value (`side as u64` of a `u32`). `simp`
discharges the side condition from the types' bounds. -/
theorem Scalar.cast_val_widen {ty : ScalarTy} (tgt : ScalarTy) (a : Scalar ty)
    (h : tgt.min ≤ ty.min ∧ ty.max ≤ tgt.max) : (Scalar.cast tgt a).val = a.val :=
  Scalar.cast_val tgt a ⟨Int.le_trans h.1 a.hmin, Int.le_trans a.hmax h.2⟩

/-- `blaze_auto_full [defs]`: `blaze_auto_core` with the library's whole simp set (and the
library facts of `blaze_auto_lib`) after `vcgen` in place of its `simp only`: for conditions
about `Duration`s, `==` of a type with `DecidableEq`, `if True`, and what `vcgen` leaves as
`true == b`. -/
syntax "blaze_auto_full" " [" term,* "]" : tactic

macro_rules
  | `(tactic| blaze_auto_full [$ts,*]) => do
    let ls ← ts.getElems.mapM fun t => `(Lean.Parser.Tactic.simpLemma| $t:term)
    `(tactic| (
    first
      | blaze_partial
      | apply Blaze.Result.eq_ok_of_triple
      | apply Blaze.Result.ok_of_triple
      | apply Blaze.Result.exists_ok_of_triple
      | skip
    blaze_vcs [$ls,*]
    all_goals try (set_option linter.unusedSimpArgs false in
      simp [Bool.not_eq_true', Bool.not_eq_false', Bool.ite_eq_true_distrib,
        Bool.ite_eq_false_distrib, Scalar.lt_eq_false, Scalar.le_eq_false, Scalar.gt_eq_false,
        Scalar.ge_eq_false, Scalar.eq_false_eq, Scalar.ne_false_eq, core.cmp.PartialEq.ne,
        Scalar.saturating_add, Scalar.saturating_sub, Scalar.cast_self, Scalar.cast_val_widen,
        ScalarTy.min, ScalarTy.max, $ls,*] at *)
    all_goals try
      grind [ScalarTy.min, ScalarTy.max, ScalarTy.bits, Scalar.ofNat_val, Scalar.add, Scalar.sub, Scalar.mul,
        Scalar.checked, List.length_pos_iff, List.head?_eq_getElem?]
    blaze_all_closed))

namespace AutoImpl

open Lean Elab Tactic Meta

/-- The most cases `blaze_auto` splits a goal into: past it, an argument is left whole and the
error says which. -/
def maxCases : Nat := 64

/-- The most constructors an inductive type of the model may have to be split on. -/
def maxCtors : Nat := 8

/-- How deep `blaze_auto` (with no list) follows calls from the goal into the models. -/
def maxDepth : Nat := 32

/-- The most definitions `blaze_auto` (with no list) unfolds. -/
def maxDefs : Nat := 512

/-- A constant generated from Rust: declared in a module under `BlazeModel`. -/
def inModel (n : Name) : MetaM Bool := do
  match ← findModuleOf? n with
  | some m => return m.getRoot == `BlazeModel
  | none => return false

/-- A definition of a model `blaze_auto` unfolds: a function of the model, not a matcher,
recursor, projection, instance or type, and not a name the compiler made up. -/
def unfoldable (n : Name) (info : DefinitionVal) : MetaM Bool := do
  let env ← getEnv
  if n.isInternal || n.hasMacroScopes then return false
  if isMatcherCore env n || isAuxRecursor env n || isNoConfusion env n then return false
  if env.isProjectionFn n then return false
  if (← isInstance n) then return false
  if (← isTypeFormerType info.type) then return false
  inModel n

/-- An opaque function giving a `Result`, which `vcgen` would otherwise find no spec for. -/
def opaqueResult (info : OpaqueVal) : MetaM Bool := do
  unless info.levelParams.isEmpty do return false
  forallTelescope info.type fun _ b => return b.isAppOf ``Blaze.Result

/-- What `blaze_auto` with no list unfolds: the model definitions the goal names and those they
call, to `maxDepth` and at most `maxDefs`; and the opaque functions met on the way. -/
structure Closure where
  defs : Array Name := #[]
  opaques : Array Name := #[]
  truncated : Option MessageData := none

/-- The definitions and opaque functions reachable from `roots` through the models' bodies. -/
def closure (roots : Array Name) : MetaM Closure := do
  let env ← getEnv
  let mut seen : NameSet := {}
  let mut frontier := roots
  let mut out : Closure := {}
  for _ in [0:maxDepth] do
    if frontier.isEmpty then break
    let mut next := #[]
    for c in frontier do
      if seen.contains c then continue
      seen := seen.insert c
      match env.find? c with
      | some (.defnInfo info) =>
        if (← unfoldable c info) then
          if out.defs.size < maxDefs then
            out := { out with defs := out.defs.push c }
            next := next ++ info.value.getUsedConstants
          else
            out := { out with truncated := some m!"it unfolds at most {maxDefs} definitions" }
      | some (.opaqueInfo info) =>
        if (← opaqueResult info) then out := { out with opaques := out.opaques.push c }
      | _ => pure ()
    frontier := next.filter (!seen.contains ·)
  unless frontier.isEmpty do
    out := { out with truncated := some m!"it follows calls {maxDepth} deep" }
  return out

/-- How many cases splitting a local of type `ty` gives: `Bool`, `Option`, a Rust `Result`, or
an inductive type of the model with at most `maxCtors` constructors and no recursion. -/
def finiteArity (ty : Expr) : MetaM (Option Nat) := do
  let ty ← whnfR (← instantiateMVars ty)
  let .const n _ := ty.getAppFn | return none
  if n == ``Bool || n == ``Option || n == ``Blaze.core.result.Result then return some 2
  let env ← getEnv
  let some (.inductInfo i) := env.find? n | return none
  if i.isRec || i.numIndices != 0 || i.ctors.isEmpty || i.ctors.length > maxCtors then
    return none
  if isStructure env n then return none
  unless (← inModel n) do return none
  return some i.ctors.length

/-- The locals to split, in order, each at most once, while the cases stay within `maxCases`;
and the locals left whole because splitting them would pass it. -/
def plan (g : MVarId) : MetaM (Array Name × Array Name × Nat) := g.withContext do
  let mut split := #[]
  let mut whole := #[]
  let mut cases := 1
  for d in (← getLCtx) do
    if d.isImplementationDetail || d.isLet then continue
    if (← isProp d.type) then continue
    let some k ← finiteArity d.type | continue
    if cases * k ≤ maxCases then
      split := split.push d.userName
      cases := cases * k
    else
      whole := whole.push d.userName
  return (split, whole, cases)

/-- Split `g` on each named local by its constructors. -/
def splitOn (g : MVarId) (names : Array Name) : MetaM (Array MVarId) := do
  let mut gs := #[g]
  for n in names do
    let mut next := #[]
    for g in gs do
      match (← g.getDecl).lctx.findFromUserName? n with
      | some d =>
        let subs ← g.cases d.fvarId
        next := next ++ subs.map (·.mvarId)
      | none => next := next.push g
    gs := next
  return gs

/-- Unfold a goal that is a statement (`f.spec.statement`, a definition of a `Prop` with no
arguments) and introduce its quantifiers, again while the goal is such a name. -/
def openStatement : TacticM Unit := do
  for _ in [0:4] do
    evalTactic (← `(tactic| intros))
    let g ← getMainGoal
    let tgt ← instantiateMVars (← g.getType)
    let .const c ls := tgt | return
    let some (.defnInfo info) := (← getEnv).find? c | return
    unless info.type.isProp do return
    replaceMainGoal [← g.replaceTargetDefEq (info.value.instantiateLevelParams info.levelParams ls)]

/-- Give `g` a hypothesis `∀ args, Triple (f args) False post epost` for each opaque `f`, and
their names. -/
def withOpaqueSpecs (g : MVarId) (fs : Array Name) : MetaM (MVarId × Array Ident) := do
  let mut g := g
  let mut ids := #[]
  for f in fs do
    let info ← getConstInfo f
    let pf ← forallTelescope info.type fun xs b => do
      mkLambdaFVars xs (mkAppN (mkConst ``Blaze.Result.false_spec) #[b.appArg!, mkAppN (mkConst f) xs])
    let n := Name.mkSimple s!"blaze_opaque_{ids.size}"
    let (_, g') ← (← g.assert n (← inferType pf) pf).intro1P
    g := g'
    ids := ids.push (mkIdent n)
  return (g, ids)

/-- Run `tac` on `g` alone with a fresh heartbeat budget; `none` when it closed `g`, else why
not. Nothing it did stays when it fails. -/
def attempt (g : MVarId) (tac : TacticM Unit) : TacticM (Option MessageData) := do
  let s ← saveState
  tryCatchRuntimeEx
    (withCurrHeartbeats do
      setGoals [g]
      tac
      let gs ← getGoals
      unless gs.isEmpty do
        throwError m!"{gs.length} goal(s) left:{indentD (MessageData.joinSep (gs.map MessageData.ofGoal) m!"\n\n")}"
      return none)
    (fun e => do
      let msg := e.toMessageData
      s.restore (restoreInfo := true)
      return some msg)

/-- Close one case: a closed goal by evaluation (`rfl`, or `∃ fuel` with fuel `64`); then `simp`
with the definitions and the library's simp set; then the pipeline (`vcgen`, `grind`) with the
definitions and a `False` spec for each opaque function. `none` when closed, else the
pipeline's error. -/
def closeCase (g : MVarId) (us : Array Term) (opaques : Array Name) :
    TacticM (Option (MessageData × MessageData)) := do
  let tgt ← instantiateMVars (← g.getType)
  let fuel := tgt.isAppOfArity ``Exists 2 && tgt.appFn!.appArg!.isConstOf ``Nat
  if !tgt.hasFVar && !tgt.hasMVar then
    if (← attempt g (evalTactic (← `(tactic| rfl)))).isNone then return none
    if fuel then
      if (← attempt g (evalTactic (← `(tactic| (refine ⟨64, ?_⟩; rfl))))).isNone then return none
  let ls ← us.mapM fun t => `(Lean.Parser.Tactic.simpLemma| $t:term)
  let simpTac ← `(tactic| open Std.WP Lean.Order in
    (set_option linter.unusedSimpArgs false in
      simp [core.cmp.PartialEq.ne, Scalar.saturating_add, Scalar.saturating_sub,
        Scalar.cast_self, Scalar.cast_val_widen, ScalarTy.min, ScalarTy.max, $ls,*]))
  if (← attempt g (evalTactic simpTac)).isNone then return none
  let core ← attempt g do
    let (g', ids) ← withOpaqueSpecs g opaques
    setGoals [g']
    let vs := us ++ ids.map (⟨·.raw⟩ : Ident → Term)
    evalTactic (← `(tactic| open Std.WP Lean.Order in
      (set_option linter.unusedSimpArgs false in blaze_auto_core [$vs,*])))
  let some why := core | return none
  let full ← attempt g do
    let (g', ids) ← withOpaqueSpecs g opaques
    setGoals [g']
    let vs := us ++ ids.map (⟨·.raw⟩ : Ident → Term)
    evalTactic (← `(tactic| open Std.WP Lean.Order in
      (set_option linter.unusedSimpArgs false in blaze_auto_full [$vs,*])))
  match full with
  | none => return none
  | some why' => return some (why, m!"{why}\n\nwith the whole simp set:\n{why'}")

/-- `blaze_auto`, with the definitions to unfold given (`some`) or found from the goal. -/
def runAuto (defsGiven : Option (Array Term)) : TacticM Unit := do
  openStatement
  let g ← getMainGoal
  let roots ← g.withContext do
    let mut cs := (← instantiateMVars (← g.getType)).getUsedConstants
    for d in (← getLCtx) do
      unless d.isImplementationDetail do
        cs := cs ++ (← instantiateMVars d.type).getUsedConstants
    return cs
  let clo ← closure roots
  let us : Array Term := defsGiven.getD (clo.defs.map fun n => ⟨(mkCIdent n).raw⟩)
  let (split, whole, _) ← plan g
  let cases ← splitOn g split
  let mut failed : Array (Nat × MVarId × (MessageData × MessageData)) := #[]
  for h : i in [0:cases.size] do
    let c := cases[i]
    if let some why ← closeCase c us clo.opaques then
      failed := failed.push (i, c, why)
  if failed.isEmpty then
    setGoals []
    return
  -- A goal the split made harder than it was: try it whole before failing.
  if !split.isEmpty then
    if (← closeCase g us clo.opaques).isNone then
      setGoals []
      return
  -- One case, nothing split and nothing cut short: the pipeline's own error, as before.
  if let #[(_, _, (why, _))] := failed then
    if split.isEmpty && whole.isEmpty && (defsGiven.isSome || clo.truncated.isNone) then
      throwError why
  let mut msg := m!"blaze_auto: {failed.size} of {cases.size} case(s) left open"
  unless split.isEmpty do
    msg := msg ++ m!" (split on {split.toList})"
  unless whole.isEmpty do
    msg := msg ++ m!"; not split, past {maxCases} cases: {whole.toList}"
  if let some t := clo.truncated then
    if defsGiven.isNone then msg := msg ++ m!"; unfolding stopped: {t}"
  for (i, c, (_, why)) in failed.toList.take 3 do
    msg := msg ++ m!"\n\ncase {i + 1}:{indentD (MessageData.ofGoal c)}\n{why}"
  if failed.size > 3 then
    msg := msg ++ m!"\n\n... and {failed.size - 3} more"
  throwError msg

end AutoImpl

/-- `blaze_auto [defs]` proves a statement about a model: the generated `f.spec.statement`
(`∀ args, f args = Result.ok true`, or a closed one with `∃ fuel`), a Hoare triple, the generated
partial-correctness `match`, or total correctness (`f args = .ok v`, `∃ r, f args = .ok r ∧ Q r`,
or `∃ r, f args = .ok r`). It needs nothing opened at its call site.

1. It unfolds a statement name and introduces its arguments.
2. It splits the arguments (and any local) of a finite type by their constructors, in order,
   each once: `Bool`, `Option`, a Rust `Result`, an enum of the model with at most 8
   constructors; at most 64 cases, past which an argument is left whole and the error says so.
3. It closes each case on its own, with a fresh heartbeat budget: a closed goal by evaluation
   (`rfl`; `∃ fuel` at fuel 64); then `simp` with `defs` and the library's simp set; then the
   pipeline `blaze_auto_core`: the goal as a triple, `vcgen` (unfolding `defs`, using the
   `@[spec]`s and any triple in `defs`, such as an induction hypothesis or a loop's invariant
   lemma), `defs` unfolded in what is left, Rust's `&&`, `||` and `!` opened into propositions
   about the integers' values, and `grind` over their bounds. Each opaque function the model
   calls gets the spec `Result.false_spec` (precondition `False`): a call no case reaches does
   not stop `vcgen`, and a call some case reaches fails the proof.
4. Without `[defs]` it unfolds the goal's model and every model definition it calls (declared
   under `BlazeModel`), at most 32 calls deep and 512 definitions; `[defs]` unfolds exactly
   `defs` (hand lemmas such as a loop's specification go there).

It fails naming each case it left open and the goals its pipeline left. -/
syntax "blaze_auto" (" [" term,* "]")? : tactic

elab_rules : tactic
  | `(tactic| blaze_auto) => AutoImpl.runAuto none
  | `(tactic| blaze_auto [$ts,*]) => AutoImpl.runAuto (some ts.getElems)

end Blaze
