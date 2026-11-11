import BlazeDiff.Sound

/-!
The differentiator is correct: where the rules claim `e` differentiable by variable `x`
(`DiffAt`), the meaning of `diff c x e` is the derivative of the meaning of `e` along `x`.

`un_sound`, `bin_sound`, `powi_sound`, `mulAdd_sound` and `select_sound` tie the generated
table to the lemmas of `Sound.lean`: for each operation, the generated template evaluated at the
node's values is, by unfolding alone (`exact`), the tangent that operation's lemma is about. An
operation whose rule changed in `rules.json`, or a new operation, fails here by name.
-/

noncomputable section

namespace BlazeDiff

/-- A ruled operation of one argument. -/
theorem un_sound (c : ℝ) (p : Prop) (op : Op) (hop : Rules.unary op = true)
    {f : ℝ → ℝ} {f' x v : ℝ} (hf : HasDerivAt f f' x) (hv : f x = v)
    (hd : (Rules.domain op).Holds noExp v 0) :
    HasDerivAt (fun t => evalUn op (f t))
      (P.evalR
        { a0 := v, a1 := 0, a2 := 0, t0 := f', t1 := 0, t2 := 0, out := evalUn op v,
          choice := c, exponent := noExp, cond := p }
        (Rules.tangent op)) x := by
  subst hv
  cases op
  case neg => exact rule_sound_neg hf
  case sqrt => exact rule_sound_sqrt c hf hd
  case exp => exact rule_sound_exp hf
  case ln => exact rule_sound_ln hf hd
  case sin => exact rule_sound_sin hf
  case cos => exact rule_sound_cos hf
  case tan => exact rule_sound_tan hf hd
  case tanh => exact rule_sound_tanh hf
  case abs => exact rule_sound_abs c hf hd
  all_goals exact absurd hop (by decide)

/-- A ruled operation of two arguments. -/
theorem bin_sound (c : ℝ) (p : Prop) (op : Op) (hop : Rules.binary op = true)
    {f g : ℝ → ℝ} {f' g' x u v : ℝ} (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x)
    (hu : f x = u) (hv : g x = v) (hd : (Rules.domain op).Holds noExp u v) :
    HasDerivAt (fun t => evalBin op (f t) (g t))
      (P.evalR
        { a0 := u, a1 := v, a2 := 0, t0 := f', t1 := g', t2 := 0, out := evalBin op u v,
          choice := c, exponent := noExp, cond := p }
        (Rules.tangent op)) x := by
  subst hu
  subst hv
  cases op
  case add => exact rule_sound_add hf hg
  case sub => exact rule_sound_sub hf hg
  case mul => exact rule_sound_mul hf hg
  case div => exact rule_sound_div hf hg hd
  case powf => exact rule_sound_powf hf hg hd
  case min => exact rule_sound_min c hf hg hd
  case max => exact rule_sound_max c hf hg hd
  all_goals exact absurd hop (by decide)

theorem powi_sound (c : ℝ) (p : Prop) (n : Blaze.I32) {f : ℝ → ℝ} {f' x v : ℝ}
    (hf : HasDerivAt f f' x) (hv : f x = v) (hd : (Rules.domain .powi).Holds n v 0) :
    HasDerivAt (fun t => f t ^ n.val)
      (P.evalR
        { a0 := v, a1 := 0, a2 := 0, t0 := f', t1 := 0, t2 := 0, out := v ^ n.val,
          choice := c, exponent := n, cond := p }
        (Rules.tangent .powi)) x := by
  subst hv
  exact rule_sound_powi n hf hd

theorem mulAdd_sound (c : ℝ) (p : Prop) {f g k : ℝ → ℝ} {f' g' k' x u v w : ℝ}
    (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x) (hk : HasDerivAt k k' x)
    (hu : f x = u) (hv : g x = v) (hw : k x = w) :
    HasDerivAt (fun t => f t * g t + k t)
      (P.evalR
        { a0 := u, a1 := v, a2 := w, t0 := f', t1 := g', t2 := k', out := u * v + w,
          choice := c, exponent := noExp, cond := p }
        (Rules.tangent .mulAdd)) x := by
  subst hu
  subst hv
  subst hw
  exact rule_sound_mulAdd hf hg hk

/-- A `select` whose condition is steady: `q` is the condition at the point, `u0` and `d0`
whatever the condition is as a float and its "derivative" (the rule reads neither). -/
theorem select_sound (c u0 d0 o : ℝ) {p : ℝ → Prop} {q : Prop} {f g : ℝ → ℝ} {f' g' x u v : ℝ}
    (hp : ∀ᶠ t in nhds x, (p t ↔ q))
    (hf : q → HasDerivAt f f' x) (hg : ¬q → HasDerivAt g g' x) :
    HasDerivAt (fun t => pick (p t) (f t) (g t))
      (P.evalR
        { a0 := u0, a1 := u, a2 := v, t0 := d0, t1 := f', t2 := g', out := o,
          choice := c, exponent := noExp, cond := q }
        (Rules.tangent .select)) x :=
  rule_sound_select hp hf hg

/-- **The differentiator is correct.** At a point where the rules claim `e` differentiable by
variable `x`, the function `t ↦ e` at the point with coordinate `x` moved to `t` has, at the
point's own coordinate, the derivative that `diff c x e` evaluates to, whatever value `c` is
chosen for kinks (off a kink it is not read). -/
theorem diff_correct (c : ℝ) (env : ℕ → ℝ) (x : ℕ) (e : Expr) (h : DiffAt env x e) :
    HasDerivAt (fun t => e.eval (Function.update env x t)) ((diff c x e).eval env) (env x) := by
  induction e with
  | const r => exact hasDerivAt_const (env x) r
  | var i =>
    by_cases hi : i = x
    · have hfun : (fun t : ℝ => (Expr.var i).eval (Function.update env x t)) = fun t => t := by
        funext t
        show Function.update env x t i = t
        rw [hi]
        simp
      have hval : (diff c x (Expr.var i)).eval env = 1 := by
        show (if i = x then Expr.const 1 else Expr.const 0).eval env = 1
        rw [if_pos hi]
        rfl
      rw [hfun, hval]
      exact hasDerivAt_id' (env x)
    · have hfun : (fun t : ℝ => (Expr.var i).eval (Function.update env x t)) =
          fun _ => env i := by
        funext t
        show Function.update env x t i = env i
        simp [hi]
      have hval : (diff c x (Expr.var i)).eval env = 0 := by
        show (if i = x then Expr.const 1 else Expr.const 0).eval env = 0
        rw [if_neg hi]
        rfl
      rw [hfun, hval]
      exact hasDerivAt_const (env x) (env i)
  | bool b => exact False.elim h
  | powi n a ih =>
    obtain ⟨ha, hd⟩ := h
    rw [eval_diff_powi]
    exact powi_sound c (a.holds env) n (ih ha) (eval_update_self env x a) hd
  | un op a ih =>
    obtain ⟨hop, ha, hd⟩ := h
    rw [eval_diff_un]
    exact un_sound c (a.holds env) op hop (ih ha) (eval_update_self env x a) hd
  | bin op a b iha ihb =>
    obtain ⟨hop, ha, hb, hd⟩ := h
    rw [eval_diff_bin]
    exact bin_sound c (a.holds env) op hop (iha ha) (ihb hb) (eval_update_self env x a)
      (eval_update_self env x b) hd
  | ter op a b d iha ihb ihd =>
    rw [eval_diff_ter]
    rcases h with ⟨hop, ha, hb, hd⟩ | ⟨hop, hsteady, hb, hd⟩
    · subst hop
      exact mulAdd_sound c (a.holds env) (iha ha) (ihb hb) (ihd hd) (eval_update_self env x a)
        (eval_update_self env x b) (eval_update_self env x d)
    · subst hop
      exact select_sound c (a.eval env) ((diff c x a).eval env) _ (hsteady rfl)
        (fun hq => ihb (hb hq)) (fun hq => ihd (hd hq))

/-- The gradient: the derivative by each variable asked. -/
theorem gradient_correct (c : ℝ) (env : ℕ → ℝ) (vars : List ℕ) (e : Expr)
    (h : ∀ x ∈ vars, DiffAt env x e) :
    ∀ x ∈ vars,
      HasDerivAt (fun t => e.eval (Function.update env x t)) ((diff c x e).eval env) (env x) :=
  fun x hx => diff_correct c env x e (h x hx)

/-- The Jacobian: entry `(e, x)` is the derivative of root `e` by variable `x`. -/
theorem jacobian_correct (c : ℝ) (env : ℕ → ℝ) (vars : List ℕ) (roots : List Expr)
    (h : ∀ e ∈ roots, ∀ x ∈ vars, DiffAt env x e) :
    ∀ e ∈ roots, ∀ x ∈ vars,
      HasDerivAt (fun t => e.eval (Function.update env x t)) ((diff c x e).eval env) (env x) :=
  fun e he x hx => diff_correct c env x e (h e he x hx)

/-- A Hessian entry, the Jacobian of the gradient: where the gradient's entry by `x` is itself
claimed differentiable by `y`, differentiating it again gives its derivative by `y`. (That this
is the second partial derivative of `e` needs `diff_correct` near the point, not only at it;
symmetry is Mathlib's and is not used.) -/
theorem hessian_entry_correct (c : ℝ) (env : ℕ → ℝ) (x y : ℕ) (e : Expr)
    (h : DiffAt env y (diff c x e)) :
    HasDerivAt (fun t => (diff c x e).eval (Function.update env y t))
      ((diff c y (diff c x e)).eval env) (env y) :=
  diff_correct c env y (diff c x e) h

end BlazeDiff
