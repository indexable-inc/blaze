import Blaze.ThreadsAuto
import Blaze.ThreadsWeakAuto
import BlazeModel.blaze_core.handoff.no_hand_back_is_missed

/-!
No hand-back is missed (`blaze_core::handoff::no_hand_back_is_missed`), on every schedule and
every pick of the weak-memory world (`Prog.valueWeak`), over the models blaze emits from the real
`hands`, `Wake::wake` and `Wake::drain`.

The flag (`Wake::woken`) is cell 0, made first; the slot cell 1. The sender stores the slot
(relaxed) and swaps the flag to `true` (AcqRel): it must wake the driver where the flag was
clear. The driver swaps the flag to `false` (AcqRel) and loads the slot (relaxed).

Why: the two swaps are updates of one cell, so they read different messages, the second the
first's (the first glues the one it read). If the sender's is first, the driver's reads the
sender's message and acquires its view, which names the sender's store of the slot, so the
driver's load cannot read the slot's first message. If the driver's is first, the sender's reads
its `false`: the sender is told to wake.

The table below (`Good`) is over two functions from program points to programs that have the
shapes `Shapes` states; the hand-written model (`BlazeCore.HandoffModel`) and the
emitted one each give theirs, and `wake_ends` is the run's end for both.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic

namespace BlazeCore.HandoffProof

open Blaze.Threads.Weak (Msg placeOf)

theorem one_val : (1 : U32).val = 1 := by decide
theorem wrap_one : (Scalar.wrap .U32 1).val = 1 := by decide

/-- The sender's program points. -/
inductive SPt where
  /-- Before the store of the slot (the sender as called). -/
  | start
  /-- Before the flag's swap (`Wake::wake`). -/
  | swap
  /-- Returned: whether it must wake the driver. -/
  | ret (must : Bool)

/-- The driver's program points. -/
inductive DPt where
  /-- Before the flag's swap (`Wake::drain`; the driver as called). -/
  | start
  /-- Before the load of the slot. -/
  | load
  /-- Returned: what it read. -/
  | ret (seen : U32)

/-- The flag's swap to `v`, as `AtomicBool.swap` asks it. -/
abbrev swapOp (v : Int) : Op := .update 0 (fun _ => some v) false .AcqRel .AcqRel

/-- Each point's shape: one operation, and the point each answer leads to. -/
structure Shapes (sAt : SPt → Prog Bool) (dAt : DPt → Prog U32) : Prop where
  sstart : sAt .start = .op 0 0 (.store 1 (1 : U32).val .Relaxed) (fun _ => sAt .swap)
  sswap : sAt .swap = .op 0 0 (swapOp 1) (fun got => sAt (.ret (!(got.value != 0))))
  sret : ∀ must, sAt (.ret must) = .done (.ok must)
  dstart : dAt .start = .op 0 0 (swapOp 0) (fun _ => dAt .load)
  dload : dAt .load = .op 0 0 (.load 1 .Relaxed) (fun got => dAt (.ret (Scalar.wrap .U32 got.value)))
  dret : ∀ seen, dAt (.ret seen) = .done (.ok seen)

/-! The abstraction of the world the invariant is over. -/

/-- The slot holds only its first message, not glued and older than every message to come. -/
def SlotOld (w : Weak.World) : Prop :=
  ∃ m0 : Msg, w.cells[1]? = some [m0] ∧ m0.glued = false ∧ m0.id < w.next

/-- The slot holds its first message and the sender's, 1. -/
def SlotNew (w : Weak.World) : Prop :=
  ∃ m0 s : Msg, w.cells[1]? = some [m0, s] ∧ s.value = 1

/-- The slot has messages. -/
def SlotSome (w : Weak.World) : Prop :=
  ∃ msgs : List Msg, w.cells[1]? = some msgs ∧ msgs ≠ []

/-- The flag holds only its first message, not glued: no swap yet. -/
def Flag0 (w : Weak.World) : Prop :=
  ∃ f0 : Msg, w.cells[0]? = some [f0] ∧ f0.glued = false

/-- The flag holds its first message, glued, and the first swap's, of which `P` holds. -/
def Flag1 (w : Weak.World) (P : Msg → Prop) : Prop :=
  ∃ f0 x : Msg, w.cells[0]? = some [f0, x] ∧ f0.glued = true ∧ x.glued = false ∧ P x

/-- The sender has done no relaxed read: rule R1 lets its relaxed store through. -/
def Quiet (w : Weak.World) : Prop := ∀ tc ∈ w.loaded, tc.1 ≠ 1

theorem SlotOld.keep {w w' : Weak.World} (h : SlotOld w) (hc : w'.cells[1]? = w.cells[1]?)
    (hn : w.next ≤ w'.next) : SlotOld w' := by
  obtain ⟨m0, h0, hg, hid⟩ := h
  exact ⟨m0, hc.trans h0, hg, by omega⟩

theorem SlotNew.keep {w w' : Weak.World} (h : SlotNew w) (hc : w'.cells[1]? = w.cells[1]?) :
    SlotNew w' := by
  obtain ⟨m0, s, h0, hs⟩ := h
  exact ⟨m0, s, hc.trans h0, hs⟩

theorem SlotSome.keep {w w' : Weak.World} (h : SlotSome w) (hc : w'.cells[1]? = w.cells[1]?) :
    SlotSome w' := by
  obtain ⟨msgs, h0, hne⟩ := h
  exact ⟨msgs, hc.trans h0, hne⟩

theorem SlotOld.some {w : Weak.World} (h : SlotOld w) : SlotSome w := by
  obtain ⟨m0, h0, -, -⟩ := h
  exact ⟨_, h0, by simp⟩

theorem SlotNew.some {w : Weak.World} (h : SlotNew w) : SlotSome w := by
  obtain ⟨m0, s, h0, -⟩ := h
  exact ⟨_, h0, by simp⟩

theorem Flag0.keep {w w' : Weak.World} (h : Flag0 w) (hc : w'.cells[0]? = w.cells[0]?) :
    Flag0 w' := by
  obtain ⟨f0, h1, hg⟩ := h
  exact ⟨f0, hc.trans h1, hg⟩

theorem Flag1.keep {w w' : Weak.World} {P : Msg → Prop} (h : Flag1 w P)
    (hc : w'.cells[0]? = w.cells[0]?) : Flag1 w' P := by
  obtain ⟨f0, x, h1, hg0, hgx, hP⟩ := h
  exact ⟨f0, x, hc.trans h1, hg0, hgx, hP⟩

theorem Quiet.keep {w w' : Weak.World} {c : Nat} (h : Quiet w)
    (hl : ∀ tc ∈ w'.loaded, tc ∈ w.loaded ∨ tc = (2, c)) : Quiet w' := by
  intro tc hm
  rcases hl tc hm with h' | rfl
  · exact h tc h'
  · show (2 : Nat) ≠ 1
    decide

/-! The invariant: every reachable combination of the two points and the world, over the
abstraction above. -/

inductive Good (w : Weak.World) : SPt → DPt → Prop
  | s0d0 : SlotOld w → Flag0 w → Quiet w → Good w .start .start
  | s1d0 : SlotNew w → 1 ≤ w.place 1 1 → Flag0 w → Good w .swap .start
  | s0d1 : SlotOld w → Flag1 w (·.value = 0) → Quiet w → Good w .start .load
  | s1d1 : SlotSome w → Flag1 w (·.value = 0) → Good w .swap .load
  | s0d2 (seen : U32) : SlotOld w → Flag1 w (·.value = 0) → Quiet w → Good w .start (.ret seen)
  | s1d2 (seen : U32) : Flag1 w (·.value = 0) → Good w .swap (.ret seen)
  /-- The sender's swap was first: its message's view names its store of the slot. -/
  | s2d0 (must : Bool) : SlotNew w → Flag1 w (fun x => 1 ≤ placeOf w.cells x.view 1) →
      Good w (.ret must) .start
  /-- The driver's swap read the sender's message: its view names the sender's store. -/
  | s2d1_sent (must : Bool) : SlotNew w → 1 ≤ w.place 2 1 → Good w (.ret must) .load
  /-- The driver's swap was first: the sender was told to wake. -/
  | s2d1_told : SlotSome w → Good w (.ret true) .load
  | s2d2_seen (must : Bool) (seen : U32) : seen.val = 1 → Good w (.ret must) (.ret seen)
  | s2d2_told (seen : U32) : Good w (.ret true) (.ret seen)

def Inv (sAt : SPt → Prog Bool) (dAt : DPt → Prog U32) (a : Prog Bool) (b : Prog U32)
    (w : Weak.World) : Prop :=
  ∃ sp dp, a = sAt sp ∧ b = dAt dp ∧ Good w sp dp

/-! One step lemma per operation, for every pick. -/

/-- The sender's store: the slot gets its message, which the sender's view names. -/
theorem sender_store {w : Weak.World} (pick : Nat) (hs : SlotOld w) (hq : Quiet w) :
    ∃ got w', w.step 1 pick (.store 1 (1 : U32).val .Relaxed) = some (got, w') ∧
      SlotNew w' ∧ 1 ≤ w'.place 1 1 ∧ w'.cells[0]? = w.cells[0]? := by
  obtain ⟨m0, hc, hg, hid⟩ := hs
  obtain ⟨got, w', q, m, hst, h⟩ := Weak.World.step_store (who := 1) (pick := pick)
    (v := (1 : U32).val) (order := .Relaxed) (by decide) hc (by simp) (by simp [hg])
    (Weak.World.refused_of_loaded hq)
  have hq0 : q = 0 := by have := h.lt; simp at this; omega
  subst hq0
  have hput : w'.cells[1]? = some [m0, m] := h.put
  refine ⟨got, w', hst, ⟨m0, m, hput, by rw [h.value_eq, one_val]⟩, ?_, h.cells_ne 0 (by decide)⟩
  have hne : (m0.id == m.id) = false := by rw [h.id_eq]; simp; omega
  simp only [Weak.World.place, placeOf, Weak.msgsAt, hput, h.sees]
  simp [Weak.placeIn, Weak.idxOf, hne]

/-- A swap of the flag while it holds only its first message: it reads that one. -/
theorem swap_first {w : Weak.World} {who : Nat} (hw : who ≠ 0) (pick : Nat) (v : Int)
    (hf : Flag0 w) :
    ∃ got w' f0 m', w.step who pick (swapOp v) = some (got, w') ∧ got = ⟨f0.value, true⟩ ∧
      w'.cells[0]? = some [{ f0 with glued := true }, m'] ∧
      Weak.World.Updated w w' who 0 (fun _ => some v) .AcqRel [f0] 0 f0 m' := by
  obtain ⟨f0, hc, hg⟩ := hf
  obtain ⟨got, w', q, m, m', hst, hgot, h⟩ := Weak.World.step_update (pick := pick)
    (change := fun _ => some v) (s := .AcqRel) (f := .AcqRel) hw hc (by simp) (by simp [hg])
    (fun _ => rfl) (Weak.World.refused_of_releases rfl)
  have hr := h.read
  rcases q with _ | q
  · simp at hr
    subst hr
    exact ⟨got, w', _, m', hst, hgot, h.put, h⟩
  · simp at hr

/-- A swap of the flag after the first: it reads the first swap's message (the older one is
glued), and acquires its view. -/
theorem swap_second {w : Weak.World} {who : Nat} (hw : who ≠ 0) (pick : Nat) (v : Int)
    {P : Msg → Prop} (hf : Flag1 w P) :
    ∃ got w' x, w.step who pick (swapOp v) = some (got, w') ∧ got.value = x.value ∧ P x ∧
      (∀ c, c ≠ 0 → placeOf w.cells x.view c ≤ w'.place who c) ∧
      w'.cells[1]? = w.cells[1]? := by
  obtain ⟨f0, x, hc, hg0, hgx, hP⟩ := hf
  obtain ⟨got, w', q, m, m', hst, hgot, h⟩ := Weak.World.step_update (pick := pick)
    (change := fun _ => some v) (s := .AcqRel) (f := .AcqRel) hw hc (by simp) (by simp [hgx])
    (fun _ => rfl) (Weak.World.refused_of_releases rfl)
  have hr := h.read
  have hu := h.unglued
  rcases q with _ | _ | q
  · simp at hr
    subst hr
    rw [hg0] at hu
    cases hu
  · simp at hr
    subst hr
    exact ⟨got, w', _, hst, by rw [hgot], hP, h.acquire rfl, h.cells_ne 1 (by decide)⟩
  · simp at hr

/-- The sender's swap first: its message's view names its store of the slot. -/
theorem sender_swap_first {w : Weak.World} (pick : Nat) (hf : Flag0 w) (hp : 1 ≤ w.place 1 1) :
    ∃ got w', w.step 1 pick (swapOp 1) = some (got, w') ∧
      Flag1 w' (fun x => 1 ≤ placeOf w'.cells x.view 1) ∧ w'.cells[1]? = w.cells[1]? := by
  obtain ⟨got, w', f0, m', hst, -, hput, h⟩ := swap_first (who := 1) (by decide) pick 1 hf
  exact ⟨got, w', hst, ⟨_, m', hput, rfl, h.unglued',
    Nat.le_trans hp (Nat.le_trans (h.place_ne 1 (by decide)) (h.release rfl 1))⟩,
    h.cells_ne 1 (by decide)⟩

/-- The driver's swap first: the flag gets its `false`. -/
theorem driver_swap_first {w : Weak.World} (pick : Nat) (hf : Flag0 w) :
    ∃ got w', w.step 2 pick (swapOp 0) = some (got, w') ∧ Flag1 w' (·.value = 0) ∧
      w'.cells[1]? = w.cells[1]? ∧ w.next ≤ w'.next ∧
      (∀ tc ∈ w'.loaded, tc ∈ w.loaded ∨ tc = (2, 0)) := by
  obtain ⟨got, w', f0, m', hst, -, hput, h⟩ := swap_first (who := 2) (by decide) pick 0 hf
  refine ⟨got, w', hst, ⟨_, m', hput, rfl, h.unglued', (Option.some.inj h.change_eq).symm⟩,
    h.cells_ne 1 (by decide), by rw [h.next_eq]; omega, h.loaded_sub⟩

/-! Each thread's step keeps the invariant. -/

section steps

variable {sAt : SPt → Prog Bool} {dAt : DPt → Prog U32}

theorem first_step (hS : Shapes sAt dAt) (a : Prog Bool) (b : Prog U32) (w : Weak.World)
    (pick : Nat) (h : Inv sAt dAt a b w) :
    AdvanceWeak 1 pick a w (fun a' w' => Inv sAt dAt a' b w') := by
  obtain ⟨sp, dp, rfl, rfl, hg⟩ := h
  cases hg with
  | s0d0 hs hf hq =>
    obtain ⟨got, w', hst, hs', hp, hc0⟩ := sender_store pick hs hq
    exact .of_awake hS.sstart hst rfl ⟨_, _, rfl, rfl, .s1d0 hs' hp (hf.keep hc0)⟩
  | s0d1 hs hf hq =>
    obtain ⟨got, w', hst, hs', -, hc0⟩ := sender_store pick hs hq
    exact .of_awake hS.sstart hst rfl ⟨_, _, rfl, rfl, .s1d1 hs'.some (hf.keep hc0)⟩
  | s0d2 seen hs hf hq =>
    obtain ⟨got, w', hst, -, -, hc0⟩ := sender_store pick hs hq
    exact .of_awake hS.sstart hst rfl ⟨_, _, rfl, rfl, .s1d2 seen (hf.keep hc0)⟩
  | s1d0 hs hp hf =>
    obtain ⟨got, w', hst, hf', hc1⟩ := sender_swap_first pick hf hp
    exact .of_awake hS.sswap hst rfl ⟨_, _, rfl, rfl, .s2d0 _ (hs.keep hc1) hf'⟩
  | s1d1 hsome hf =>
    obtain ⟨got, w', x, hst, hv, hx, -, hc1⟩ := swap_second (who := 1) (by decide) pick 1 hf
    have hx : x.value = 0 := hx
    have hm : sAt (.ret (!(got.value != 0))) = sAt (.ret true) := by
      rw [hv, hx]; rfl
    exact .of_awake hS.sswap hst rfl ⟨_, _, hm, rfl, .s2d1_told (hsome.keep hc1)⟩
  | s1d2 seen hf =>
    obtain ⟨got, w', x, hst, hv, hx, -, -⟩ := swap_second (who := 1) (by decide) pick 1 hf
    have hx : x.value = 0 := hx
    have hm : sAt (.ret (!(got.value != 0))) = sAt (.ret true) := by
      rw [hv, hx]; rfl
    exact .of_awake hS.sswap hst rfl ⟨_, _, hm, rfl, .s2d2_told seen⟩
  | s2d0 must hs hf => exact .of_done (hS.sret must) ⟨_, _, rfl, rfl, .s2d0 must hs hf⟩
  | s2d1_sent must hs hp =>
    exact .of_done (hS.sret must) ⟨_, _, rfl, rfl, .s2d1_sent must hs hp⟩
  | s2d1_told hs => exact .of_done (hS.sret true) ⟨_, _, rfl, rfl, .s2d1_told hs⟩
  | s2d2_seen must seen hv =>
    exact .of_done (hS.sret must) ⟨_, _, rfl, rfl, .s2d2_seen must seen hv⟩
  | s2d2_told seen => exact .of_done (hS.sret true) ⟨_, _, rfl, rfl, .s2d2_told seen⟩

theorem second_step (hS : Shapes sAt dAt) (a : Prog Bool) (b : Prog U32) (w : Weak.World)
    (pick : Nat) (h : Inv sAt dAt a b w) :
    AdvanceWeak 2 pick b w (fun b' w' => Inv sAt dAt a b' w') := by
  obtain ⟨sp, dp, rfl, rfl, hg⟩ := h
  cases hg with
  | s0d0 hs hf hq =>
    obtain ⟨got, w', hst, hf', hc1, hn, hl⟩ := driver_swap_first pick hf
    exact .of_awake hS.dstart hst rfl
      ⟨_, _, rfl, rfl, .s0d1 (hs.keep hc1 hn) hf' (hq.keep hl)⟩
  | s1d0 hs _ hf =>
    obtain ⟨got, w', hst, hf', hc1, -, -⟩ := driver_swap_first pick hf
    exact .of_awake hS.dstart hst rfl ⟨_, _, rfl, rfl, .s1d1 (hs.some.keep hc1) hf'⟩
  | s2d0 must hs hf =>
    obtain ⟨got, w', x, hst, -, hx, hacq, hc1⟩ := swap_second (who := 2) (by decide) pick 0 hf
    have hx : 1 ≤ placeOf w.cells x.view 1 := hx
    exact .of_awake hS.dstart hst rfl
      ⟨_, _, rfl, rfl, .s2d1_sent must (hs.keep hc1) (Nat.le_trans hx (hacq 1 (by decide)))⟩
  | s0d1 hs hf hq =>
    obtain ⟨msgs, hc, hne⟩ := hs.some
    obtain ⟨got, w', i, m, hst, -, h⟩ :=
      Weak.World.step_load (who := 2) (pick := pick) (order := .Relaxed) (by decide) hc hne
    have hc' : ∀ c : Nat, w'.cells[c]? = w.cells[c]? := fun c => by rw [h.cells_eq]
    exact .of_awake hS.dload hst rfl ⟨_, _, rfl, rfl,
      .s0d2 _ (hs.keep (hc' 1) (Nat.le_of_eq h.next_eq.symm)) (hf.keep (hc' 0))
        (hq.keep h.loaded_sub)⟩
  | s1d1 hsome hf =>
    obtain ⟨msgs, hc, hne⟩ := hsome
    obtain ⟨got, w', i, m, hst, -, h⟩ :=
      Weak.World.step_load (who := 2) (pick := pick) (order := .Relaxed) (by decide) hc hne
    exact .of_awake hS.dload hst rfl
      ⟨_, _, rfl, rfl, .s1d2 _ (hf.keep (by rw [h.cells_eq]))⟩
  | s2d1_sent must hs hp =>
    obtain ⟨m0, s, hc, hsv⟩ := hs
    obtain ⟨got, w', i, m, hst, hgot, h⟩ :=
      Weak.World.step_load (who := 2) (pick := pick) (order := .Relaxed) (by decide) hc (by simp)
    have hi : i = 1 := by
      have h1 := h.not_older
      have h2 := Weak.lt_of_get h.read
      simp at h2
      omega
    subst hi
    have hr := h.read
    simp at hr
    subst hr
    have hseen : (Scalar.wrap .U32 got.value).val = 1 := by
      rw [hgot]
      show (Scalar.wrap .U32 s.value).val = 1
      rw [hsv]
      exact wrap_one
    exact .of_awake hS.dload hst rfl ⟨_, _, rfl, rfl, .s2d2_seen must _ hseen⟩
  | s2d1_told hsome =>
    obtain ⟨msgs, hc, hne⟩ := hsome
    obtain ⟨got, w', i, m, hst, -, -⟩ :=
      Weak.World.step_load (who := 2) (pick := pick) (order := .Relaxed) (by decide) hc hne
    exact .of_awake hS.dload hst rfl ⟨_, _, rfl, rfl, .s2d2_told _⟩
  | s0d2 seen hs hf hq => exact .of_done (hS.dret seen) ⟨_, _, rfl, rfl, .s0d2 seen hs hf hq⟩
  | s1d2 seen hf => exact .of_done (hS.dret seen) ⟨_, _, rfl, rfl, .s1d2 seen hf⟩
  | s2d2_seen must seen hv =>
    exact .of_done (hS.dret seen) ⟨_, _, rfl, rfl, .s2d2_seen must seen hv⟩
  | s2d2_told seen => exact .of_done (hS.dret seen) ⟨_, _, rfl, rfl, .s2d2_told seen⟩

/-- What the oracle needs of an end: where both returned, the driver saw the slot or the sender
was told to wake. -/
def Post : Ended Bool → Ended U32 → Weak.World → Prop
  | .Done must, .Done seen, _ => seen.val = 1 ∨ must = true
  | _, _, _ => True

theorem last_step (hS : Shapes sAt dAt) (a : Prog Bool) (b : Prog U32) (w : Weak.World)
    (h : Inv sAt dAt a b w) :
    ∃ ea eb w', (two [] a b).execWeak w = .ok ((ea, eb), w') ∧ Post ea eb w' := by
  obtain ⟨sp, dp, rfl, rfl, hg⟩ := h
  cases hg <;> simp only [hS.sstart, hS.sswap, hS.sret, hS.dstart, hS.dload, hS.dret]
  case s2d2_seen must seen hv => exact ⟨_, _, _, two_nil_ok_weak rfl rfl, Or.inl hv⟩
  case s2d2_told seen => exact ⟨_, _, _, two_nil_ok_weak rfl rfl, Or.inr rfl⟩
  all_goals exact ⟨_, _, _, two_nil_ok_weak rfl rfl, trivial⟩

/-- The world after the flag is made: it holds `flagged`. -/
def made (flagged : Bool) : Weak.World :=
  ⟨[[⟨0, if flagged then 1 else 0, [(0, 0)], false⟩]], [(0, [(0, 0)])], 1, [], [], [], [], 0, []⟩

/-- The world the threads start in: the flag (cell 0) holds `flagged`, the slot (cell 1) 0. -/
def start (flagged : Bool) : Weak.World :=
  ⟨[[⟨0, if flagged then 1 else 0, [(0, 0)], false⟩], [⟨1, 0, [(1, 1)], false⟩]],
    [(0, [(1, 1)])], 2, [], [], [], [], 0, []⟩

theorem new_flag (flagged : Bool) :
    (AtomicBool.new flagged).execWeak Weak.World.new = .ok (⟨0⟩, made flagged) := by
  cases flagged <;> rfl

theorem new_slot (flagged : Bool) :
    (Atomic.new (0 : U32)).execWeak (made flagged) = .ok (⟨1⟩, start flagged) := by
  cases flagged <;> rfl

/-- Every run of the two threads from `start` ends, and its end is one `Post` holds of. -/
theorem wake_ends (hS : Shapes sAt dAt) (schedule : List Step) (flagged : Bool) :
    ∃ ea eb w', (two schedule (sAt .start) (dAt .start)).execWeak (start flagged) =
      .ok ((ea, eb), w') ∧ Post ea eb w' :=
  two_ends_weak (fun _ => Inv sAt dAt) (fun _ => first_step hS) (fun _ => second_step hS) Post
    (fun a b w h => last_step hS a b w h) schedule _ _ _
    ⟨.start, .start, rfl, rfl,
      .s0d0 ⟨_, rfl, rfl, (by decide : 1 < 2)⟩ ⟨_, rfl, rfl⟩ (fun _ h => by simp [start] at h)⟩

end steps

/-! The emitted model: its program points. -/

open blaze_core.handoff

/-- The `Wake` both threads share: its flag is cell 0. -/
def wake : Wake := { woken := ⟨0⟩ }

def senderAt : SPt → Prog Bool
  | .start => hands.closure0.call { c0 := ⟨1⟩, c1 := wake }
  | .swap => Wake.wake wake
  | .ret must => .done (.ok must)

def driverAt : DPt → Prog U32
  | .start => hands.closure1.call { c0 := wake, c1 := ⟨1⟩ }
  | .load => Atomic.load ⟨1⟩ .Relaxed
  | .ret seen => .done (.ok seen)

theorem shapes : Shapes senderAt driverAt where
  sstart := Prog.op_ext fun _ => rfl
  sswap := Prog.op_ext fun _ => rfl
  sret := fun _ => rfl
  dstart := Prog.op_ext fun _ => rfl
  dload := Prog.op_ext fun _ => rfl
  dret := fun _ => rfl

/-- `hands.at_3`, the oracle, is `true` on every end `Post` holds of. -/
theorem at_3_true {ea : Ended Bool} {eb : Ended U32} {w : Weak.World} (h : Post ea eb w) :
    hands.at_3 (ea, eb) = .done (.ok true) := by
  cases ea with
  | Done must =>
    cases eb with
    | Done seen =>
      have h : seen.val = 1 ∨ must = true := h
      have he : Scalar.eq seen (1 : U32) = decide (seen.val = 1) := by
        simp only [Scalar.eq, one_val]
      simp only [hands.at_3, he]
      by_cases hs : seen.val = 1
      · simp [hs]
      · have hm : must = true := h.resolve_left hs
        simp [hs, hm]
    | Running => rfl
    | Asleep => rfl
  | Running => cases eb <;> rfl
  | Asleep => cases eb <;> rfl

end BlazeCore.HandoffProof

open BlazeCore.HandoffProof in
theorem blaze_core.handoff.no_hand_back_is_missed.spec.proof :
    blaze_core.handoff.no_hand_back_is_missed.spec.statement := by
  intro schedule flagged
  obtain ⟨ea, eb, w', he, hp⟩ := wake_ends shapes schedule.val flagged
  have he' : (Threads.two_of schedule
      (blaze_core.handoff.hands.closure0.call { c0 := ⟨1⟩, c1 := { woken := ⟨0⟩ } })
      (blaze_core.handoff.hands.closure1.call { c0 := { woken := ⟨0⟩ }, c1 := ⟨1⟩ })).execWeak
      (start flagged) = .ok ((ea, eb), w') := he
  apply Prog.valueWeak_of_execWeak (w := w')
  simp only [blaze_core.handoff.no_hand_back_is_missed, blaze_core.handoff.hands,
    blaze_core.handoff.hands.at_1, blaze_core.handoff.hands.at_2, Prog.execWeak_bind, new_flag,
    new_slot, he', at_3_true hp, Prog.execWeak_done_ok]
