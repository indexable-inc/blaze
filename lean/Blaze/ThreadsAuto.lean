import Blaze.Atomic

/-!
Proofs about two threads sharing atomic words (`Blaze.Threads.two`): the lemmas a proof for
every schedule is written with. Nothing here is about one program.

How a proof that something holds on every schedule of `two schedule a b` is written:

1. Name each thread's program points: an inductive of points and a function from a point to the
   `Prog` the thread still has to run there (its first point is the function's model).
2. Prove each point's shape: one `op` node whose continuation is, for each answer, the program at
   a named point (`Prog.op 0 0 o (fun got => ...)`). `Prog.op_ext` and `rfl` (or cases on the
   answer) prove it; `Atomic.load_eq`, `swap_eq`, `compare_exchange_eq`, `wait_eq`, `wake_eq`
   say what one atomic is as an `op`.
3. State the invariant `Inv n a b w`: `n` the schedule steps left, `a`, `b` the threads' points,
   `w` the world; typically an inductive listing the reachable combinations of the two points
   and the world's cells, sleepers and woken threads, and `w.OneCell c` (or every op `SeqCst`).
4. Prove each thread's step keeps it, from `Inv (n + 1)` to `Inv n` (`first`, `second` of
   `two_ends`): per combination, `Advance.of_step` with the point's shape and the
   `World.step_*` lemma of its operation, then the combination after; a thread that has returned
   stays (`Advance.of_done`).
5. Prove what each end says (`last` of `two_ends`): `two_nil_ok` with `ended_exec_*`.
6. `two_ends` gives it for every schedule; `Prog.exec_bind` puts the run in its context.
-/

namespace Blaze.Threads

open Blaze.core.sync.atomic

/-! ## Programs: `exec` and `bind` -/

namespace Prog

@[simp] theorem exec_done_ok {α : Type} (v : α) (w : World) :
    (Prog.done (.ok v)).exec w = .ok (v, w) := rfl

@[simp] theorem exec_done_fail {α : Type} (e : Error) (w : World) :
    (Prog.done (.fail e) : Prog α).exec w = .fail e := rfl

@[simp] theorem exec_done_div {α : Type} (w : World) :
    (Prog.done .div : Prog α).exec w = .div := rfl

/-- One operation of a thread of a run (`who ≠ 0`) that the world accepts: the program goes on
with the answer, asleep or not (the run's `two` decides what a sleeper does). -/
theorem exec_op {α : Type} {who pick : Nat} {o : Op} {next : Got → Prog α} {w w' : World}
    {got : Got} (hw : who ≠ 0) (h : w.step who pick o = some (got, w')) :
    (Prog.op who pick o next).exec w = (next got).exec w' := by
  simp [exec, h, hw]

/-- One operation that does not sleep, by any thread: the program goes on with the answer. -/
theorem exec_op_awake {α : Type} {who pick : Nat} {o : Op} {next : Got → Prog α}
    {w w' : World} {got : Got} (h : w.step who pick o = some (got, w'))
    (hs : o.sleeps got = false) :
    (Prog.op who pick o next).exec w = (next got).exec w' := by
  simp [exec, h, hs]

/-- `p >>= f` runs `p`, then `f` on its value in the world after; a panic or running out of
fuel passes through. -/
theorem exec_bind {α β : Type} (p : Prog α) (f : α → Prog β) (w : World) :
    (p >>= f).exec w =
      (match p.exec w with
       | .ok (v, w') => (f v).exec w'
       | .fail e => .fail e
       | .div => .div) := by
  induction p generalizing w with
  | done r => cases r <;> rfl
  | op who pick o next ih =>
    rw [bind_op]
    simp only [exec]
    rcases w.step who pick o with _ | ⟨got, w'⟩
    · rfl
    · by_cases hc : (who == 0 && o.sleeps got) = true
      · simp only [hc, ite_true]
      · simp only [hc, Bool.false_eq_true, ite_false, ih]

/-- A program whose run from the empty world ends with `v` has the value `v`. -/
theorem value_of_exec {α : Type} {p : Prog α} {v : α} {w : World}
    (h : p.exec World.new = .ok (v, w)) : p.value = .ok v := by
  simp [value, h]

/-- Two `op` nodes with the same operation and continuations equal on every answer. -/
theorem op_ext {α : Type} {who pick : Nat} {o : Op} {next next' : Got → Prog α}
    (h : ∀ got, next got = next' got) : Prog.op who pick o next = Prog.op who pick o next' := by
  rw [funext h]

/-- `ask o` and what follows it is one `op` node. -/
@[simp] theorem ask_bind {β : Type} (o : Op) (f : Got → Prog β) :
    (Prog.ask o >>= f) = Prog.op 0 0 o f := rfl

end Prog

/-! ## The world: one step of each operation -/

namespace World

variable {w : World} {who pick cell : Nat}

@[simp] theorem touched_cells (os : List Order) : (w.touched who cell os).cells = w.cells := by
  unfold touched; split <;> rfl

@[simp] theorem touched_asleep (os : List Order) :
    (w.touched who cell os).asleep = w.asleep := by
  unfold touched; split <;> rfl

@[simp] theorem touched_woken (os : List Order) : (w.touched who cell os).woken = w.woken := by
  unfold touched; split <;> rfl

/-- The threads of the run have touched no cell but `cell`: whatever their orderings, the run
is one this memory is the meaning of. -/
def OneCell (cell : Nat) (w : World) : Prop := w.used = [] ∨ w.used = [cell]

theorem OneCell.fits {os : List Order} (h : w.OneCell cell) :
    (w.touched who cell os).fits = true := by
  unfold World.touched World.fits
  rcases h with h | h <;> split <;> simp [h]

theorem OneCell.touched {os : List Order} (h : w.OneCell cell) :
    (w.touched who cell os).OneCell cell := by
  unfold World.touched OneCell
  rcases h with h | h <;> split <;> simp [h]

/-- A world of one cell holding `v`: cell 0 is `v`. -/
theorem get_single {v : Int} (h : w.cells = [v]) : w.cells[0]? = some v := by simp [h]

theorem get_single_length {v : Int} (h : w.cells = [v]) : 0 < w.cells.length := by simp [h]

theorem step_load {v : Int} {order : Order} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell [order]).fits = true) :
    w.step who pick (.load cell order) = some (⟨v, true⟩, w.touched who cell [order]) := by
  unfold step; simp [hv, hf]

theorem step_store {value : Int} {order : Order} (hc : cell < w.cells.length)
    (hf : (w.touched who cell [order]).fits = true) :
    w.step who pick (.store cell value order) =
      some (⟨0, true⟩, { w.touched who cell [order] with cells := w.cells.set cell value }) := by
  unfold World.step World.fits at *
  simp [hc, hf]

/-- A read-modify-write, as the world does it. -/
theorem step_update {old : Int} {change : Int → Option Int} {weak : Bool} {s f : Order}
    (hv : w.cells[cell]? = some old) (hf : (w.touched who cell [s, f]).fits = true) :
    w.step who pick (.update cell change weak s f) =
      if weak && pick == 1 then some (⟨old, false⟩, w.touched who cell [s, f])
      else
        match change old with
        | some new =>
          some (⟨old, true⟩, { w.touched who cell [s, f] with cells := w.cells.set cell new })
        | none => some (⟨old, false⟩, w.touched who cell [s, f]) := by
  unfold step; simp [hv, hf]; rfl

/-- A swap (and every read-modify-write that always writes `new`). -/
theorem step_swap {old new : Int} {s f : Order} (hv : w.cells[cell]? = some old)
    (hf : (w.touched who cell [s, f]).fits = true) :
    w.step who pick (.update cell (fun _ => some new) false s f) =
      some (⟨old, true⟩, { w.touched who cell [s, f] with cells := w.cells.set cell new }) := by
  rw [step_update hv hf]; simp

/-- A strong compare-and-swap that finds `cur`. -/
theorem step_cas_ok {old cur new : Int} {s f : Order} (hv : w.cells[cell]? = some old)
    (hf : (w.touched who cell [s, f]).fits = true) (h : old = cur) :
    w.step who pick (.update cell (fun x => if x = cur then some new else none) false s f) =
      some (⟨old, true⟩, { w.touched who cell [s, f] with cells := w.cells.set cell new }) := by
  rw [step_update hv hf]; simp [h]

/-- A strong compare-and-swap that does not find `cur`. -/
theorem step_cas_err {old cur new : Int} {s f : Order} (hv : w.cells[cell]? = some old)
    (hf : (w.touched who cell [s, f]).fits = true) (h : old ≠ cur) :
    w.step who pick (.update cell (fun x => if x = cur then some new else none) false s f) =
      some (⟨old, false⟩, w.touched who cell [s, f]) := by
  rw [step_update hv hf]; simp [h]

theorem step_wait_woken {v e : Int} {tm : Bool} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell []).fits = true) (hw : (who, cell) ∈ w.woken) :
    w.step who pick (.wait cell e tm) =
      some (⟨woken_code, true⟩,
        { w.touched who cell [] with woken := w.woken.erase (who, cell) }) := by
  unfold step; simp [hv, hf, hw]

theorem step_wait_spurious {v e : Int} {tm : Bool} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell []).fits = true) (hw : (who, cell) ∉ w.woken)
    (ha : (who, cell) ∈ w.asleep) (hp : pick = 1) :
    w.step who pick (.wait cell e tm) =
      some (⟨spurious_code, true⟩,
        { w.touched who cell [] with asleep := w.asleep.erase (who, cell) }) := by
  unfold step; simp [hv, hf, hw, ha, hp]

theorem step_wait_timed_out {v e : Int} {tm : Bool} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell []).fits = true) (hw : (who, cell) ∉ w.woken)
    (ha : (who, cell) ∈ w.asleep) (hp : pick = 2) (ht : tm = true) :
    w.step who pick (.wait cell e tm) =
      some (⟨timed_out_code, true⟩,
        { w.touched who cell [] with asleep := w.asleep.erase (who, cell) }) := by
  unfold step; simp [hv, hf, hw, ha, hp, ht]

/-- A sleeper the world neither wakes for nothing nor times out sleeps on. -/
theorem step_wait_stays {v e : Int} {tm : Bool} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell []).fits = true) (hw : (who, cell) ∉ w.woken)
    (ha : (who, cell) ∈ w.asleep) (hp1 : pick ≠ 1) (hp2 : ¬ (pick = 2 ∧ tm = true)) :
    w.step who pick (.wait cell e tm) = some (⟨0, false⟩, w.touched who cell []) := by
  unfold step
  simp [hv, hf, hw, ha, hp1]
  intro h2; simp_all

theorem step_wait_changed {v e : Int} {tm : Bool} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell []).fits = true) (hw : (who, cell) ∉ w.woken)
    (ha : (who, cell) ∉ w.asleep) (hne : v ≠ e) :
    w.step who pick (.wait cell e tm) = some (⟨changed_code, true⟩, w.touched who cell []) := by
  unfold step; simp [hv, hf, hw, ha, hne]

theorem step_wait_sleeps {v e : Int} {tm : Bool} (hv : w.cells[cell]? = some v)
    (hf : (w.touched who cell []).fits = true) (hw : (who, cell) ∉ w.woken)
    (ha : (who, cell) ∉ w.asleep) (heq : v = e) :
    w.step who pick (.wait cell e tm) =
      some (⟨0, false⟩, { w.touched who cell [] with asleep := (who, cell) :: w.asleep }) := by
  unfold step; simp [hv, hf, hw, ha, heq]

/-- A wake with nobody asleep on the cell. -/
theorem step_wake_none {all : Bool} (hc : cell < w.cells.length)
    (hf : (w.touched who cell []).fits = true)
    (hs : w.asleep.filter (fun it => it.2 == cell) = []) :
    w.step who pick (.wake cell all) = some (⟨0, false⟩, w.touched who cell []) := by
  unfold step; simp [hc, hf, hs]

/-- A wake of one with exactly one thread asleep on the cell: it is woken. -/
theorem step_wake_single {one : Nat × Nat} (hc : cell < w.cells.length)
    (hf : (w.touched who cell []).fits = true)
    (hs : w.asleep.filter (fun it => it.2 == cell) = [one]) :
    w.step who pick (.wake cell false) =
      some (⟨1, true⟩,
        { w.touched who cell [] with asleep := w.asleep.erase one, woken := one :: w.woken }) := by
  unfold step
  cases pick <;> simp [hc, hf, hs]

end World

/-- Why a wait returned, from its answer: what `Threads.wait` gives. -/
def Woke.ofGot (got : Got) : Woke :=
  if got.value = World.changed_code then .Changed
  else if got.value = World.spurious_code then .Spurious
  else if got.value = World.timed_out_code then .TimedOut
  else .Woken

@[simp] theorem Woke.ofGot_woken (b : Bool) : Woke.ofGot ⟨World.woken_code, b⟩ = .Woken := by
  simp [Woke.ofGot, World.woken_code, World.changed_code, World.spurious_code,
    World.timed_out_code]

@[simp] theorem Woke.ofGot_changed (b : Bool) : Woke.ofGot ⟨World.changed_code, b⟩ = .Changed := by
  simp [Woke.ofGot]

@[simp] theorem Woke.ofGot_spurious (b : Bool) :
    Woke.ofGot ⟨World.spurious_code, b⟩ = .Spurious := by
  simp [Woke.ofGot, World.changed_code, World.spurious_code]

@[simp] theorem Woke.ofGot_timed_out (b : Bool) :
    Woke.ofGot ⟨World.timed_out_code, b⟩ = .TimedOut := by
  simp [Woke.ofGot, World.changed_code, World.spurious_code, World.timed_out_code]

/-! ## Each atomic operation as one `op` -/

section shapes

variable {ty : ScalarTy}

theorem Atomic.load_eq (a : Atomic ty) {order : core.sync.atomic.Ordering} (h : order.loads = true) :
    a.load order = .op 0 0 (.load a.cell order) (fun got => .done (.ok (Scalar.wrap ty got.value))) := by
  unfold Atomic.load; simp only [h, ↓reduceIte]; rfl

theorem Atomic.store_eq (a : Atomic ty) (v : Scalar ty) {order : core.sync.atomic.Ordering}
    (h : order.stores = true) :
    a.store v order = .op 0 0 (.store a.cell v.val order) (fun _ => .done (.ok ())) := by
  unfold Atomic.store; simp only [h, ↓reduceIte]; rfl

theorem Atomic.swap_eq (a : Atomic ty) (v : Scalar ty) (order : core.sync.atomic.Ordering) :
    a.swap v order =
      .op 0 0 (.update a.cell (fun _ => some v.val) false order order)
        (fun got => .done (.ok (Scalar.wrap ty got.value))) := rfl

theorem Atomic.compare_exchange_eq (a : Atomic ty) (cur new : Scalar ty) (s : core.sync.atomic.Ordering)
    {f : core.sync.atomic.Ordering} (h : f.loads = true) :
    a.compare_exchange cur new s f =
      .op 0 0 (.update a.cell (fun x => if x = cur.val then some new.val else none) false s f)
        (fun got => .done (.ok
          (if got.flag then .Ok (Scalar.wrap ty got.value) else .Err (Scalar.wrap ty got.value)))) := by
  unfold Atomic.compare_exchange Atomic.compare; simp only [h, ↓reduceIte]; rfl

theorem wait_eq (word : Atomic .U32) (e : U32) (tm : Bool) :
    wait word e tm = .op 0 0 (.wait word.cell e.val tm) (fun got => .done (.ok (Woke.ofGot got))) :=
  rfl

theorem wake_eq (word : Atomic .U32) :
    wake word = .op 0 0 (.wake word.cell false) (fun got => .done (.ok got.flag)) := rfl

end shapes

/-! ## Two threads -/

/-- What one step of thread `who`, with the world's `pick`, from `p` in `w` must do for `P` to
hold where `two` goes on: a thread that returned stays (`P p w`); one at an operation has it
untagged, the world accepts it, and `P` holds of where the thread then stands (the same `op`
where it sleeps) and the world after. -/
def Advance {α : Type} (who pick : Nat) (p : Prog α) (w : World)
    (P : Prog α → World → Prop) : Prop :=
  match p with
  | .done r => P (.done r) w
  | .op tagged k o next =>
    tagged = 0 ∧ ∃ got w', w.step who pick o = some (got, w') ∧
      P (if o.sleeps got then .op tagged k o next else next got) w'

/-- A step from a point whose shape is `hp`, by the world's step `hs`. -/
theorem Advance.of_step {α : Type} {who pick k : Nat} {p : Prog α} {o : Op}
    {next : Got → Prog α} {w w' : World} {got : Got} {P : Prog α → World → Prop}
    (hp : p = .op 0 k o next) (hs : w.step who pick o = some (got, w'))
    (hP : P (if o.sleeps got then p else next got) w') : Advance who pick p w P := by
  subst hp; exact ⟨rfl, got, w', hs, hP⟩

/-- A thread that has returned. -/
theorem Advance.of_done {α : Type} {who pick : Nat} {p : Prog α} {r : Result α} {w : World}
    {P : Prog α → World → Prop} (hp : p = .done r) (hP : P p w) : Advance who pick p w P := by
  subst hp; exact hP

/-- The induction over a schedule: an invariant over the steps left, the two threads' programs
and the world that each thread's step keeps (from `n + 1` steps left to `n`) holds when the
schedule ends, and the run is the run of the end from there. -/
theorem two_exec {α β : Type} (Inv : Nat → Prog α → Prog β → World → Prop)
    (first : ∀ n a b w pick, Inv (n + 1) a b w → Advance 1 pick a w (fun a' w' => Inv n a' b w'))
    (second : ∀ n a b w pick, Inv (n + 1) a b w → Advance 2 pick b w (fun b' w' => Inv n a b' w')) :
    ∀ (schedule : List Step) (a : Prog α) (b : Prog β) (w : World), Inv schedule.length a b w →
      ∃ a' b' w', Inv 0 a' b' w' ∧ (two schedule a b).exec w = (two [] a' b').exec w' := by
  intro schedule
  induction schedule with
  | nil => intro a b w h; exact ⟨a, b, w, h, rfl⟩
  | cons step rest ih =>
    intro a b w h
    simp only [List.length_cons] at h
    simp only [two]
    split
    · have hb := second rest.length a b w step.pick.val.toNat h
      cases b with
      | done r => exact ih a (.done r) w hb
      | op tg k o next =>
        obtain ⟨rfl, got, w', hs, hP⟩ := hb
        obtain ⟨a', b', w'', hI, he⟩ := ih _ _ _ hP
        refine ⟨a', b', w'', hI, ?_⟩
        simp only [turn, bne_self_eq_false, Bool.false_eq_true, ite_false]
        rw [Prog.exec_op (by decide) hs]
        exact he
    · have ha := first rest.length a b w step.pick.val.toNat h
      cases a with
      | done r => exact ih (.done r) b w ha
      | op tg k o next =>
        obtain ⟨rfl, got, w', hs, hP⟩ := ha
        obtain ⟨a', b', w'', hI, he⟩ := ih _ _ _ hP
        refine ⟨a', b', w'', hI, ?_⟩
        simp only [turn, bne_self_eq_false, Bool.false_eq_true, ite_false]
        rw [Prog.exec_op (by decide) hs]
        exact he

/-- For every schedule: with `Inv` kept by each thread's step (`first`, `second`) and every end
in `Inv 0` satisfying `Post` (`last`), the run of `two schedule a b` from `Inv` ends, and how
each thread stood and the world satisfy `Post`. -/
theorem two_ends {α β : Type} (Inv : Nat → Prog α → Prog β → World → Prop)
    (first : ∀ n a b w pick, Inv (n + 1) a b w → Advance 1 pick a w (fun a' w' => Inv n a' b w'))
    (second : ∀ n a b w pick, Inv (n + 1) a b w → Advance 2 pick b w (fun b' w' => Inv n a b' w'))
    (Post : Ended α → Ended β → World → Prop)
    (last : ∀ a b w, Inv 0 a b w →
      ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧ Post ea eb w')
    (schedule : List Step) (a : Prog α) (b : Prog β) (w : World)
    (h : Inv schedule.length a b w) :
    ∃ ea eb w', (two schedule a b).exec w = .ok ((ea, eb), w') ∧ Post ea eb w' := by
  obtain ⟨a', b', w', hI, he⟩ := two_exec Inv first second schedule a b w h
  obtain ⟨ea, eb, w'', he', hP⟩ := last a' b' w' hI
  exact ⟨ea, eb, w'', he.trans he', hP⟩

/-- The end of a schedule: how thread 1 stands, then thread 2. -/
theorem two_nil_ok {α β : Type} {a : Prog α} {b : Prog β} {w w1 w2 : World} {ea : Ended α}
    {eb : Ended β} (ha : (ended 1 a).exec w = .ok (ea, w1)) (hb : (ended 2 b).exec w1 = .ok (eb, w2)) :
    (two [] a b).exec w = .ok ((ea, eb), w2) := by
  simp only [two, Prog.exec_bind, ha, hb]
  rfl

@[simp] theorem ended_exec_done {α : Type} (who : Nat) (v : α) (w : World) :
    (ended who (.done (.ok v))).exec w = .ok (.Done v, w) := rfl

/-- A thread at an operation that is not a wait can go on. -/
theorem ended_exec_running {α : Type} {who t k : Nat} {o : Op} {next : Got → Prog α} (w : World)
    (ho : ∀ c e tm, o ≠ .wait c e tm) : (ended who (.op t k o next)).exec w = .ok (.Running, w) := by
  cases o with
  | wait c e tm => exact absurd rfl (ho c e tm)
  | _ => rfl

/-- A thread at a wait: asleep with no wake coming where it was not woken and it sleeps, or
would go to sleep. -/
theorem ended_exec_wait {α : Type} {who t k cell : Nat} {e v : Int} {tm : Bool}
    {next : Got → Prog α} {w : World} (hv : w.cells[cell]? = some v) :
    (ended who (.op t k (.wait cell e tm) next)).exec w =
      .ok ((if (who, cell) ∉ w.woken ∧ ((who, cell) ∈ w.asleep ∨ v = e) then .Asleep
        else .Running), w) := by
  simp only [ended]
  simp [Prog.exec, World.step, hv, Op.sleeps]

end Blaze.Threads
