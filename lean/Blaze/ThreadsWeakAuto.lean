import Blaze.ThreadsWeak

/-!
Proofs about two threads in the weak-memory world (`Blaze.Threads.Weak`): the lemmas a proof for
every schedule and every pick is written with, each the weak counterpart of one in
`Blaze.ThreadsAuto`. Nothing here is about one program.

How a proof is written is `Blaze.ThreadsAuto`'s, with `execWeak`, `AdvanceWeak`, `two_ends_weak`
and these step lemmas. The world is not tracked term by term (message ids and views depend on
the order the threads ran in) but through what a step lemma says of it:

- `msgsAt cells c`: the messages of cell `c`, oldest first.
- `placeOf cells v c`: the place in cell `c` a view `v` names (0 where it names none).
- `w.place t c`: the place of thread `t`'s view of cell `c`: no read of `c` by `t` is older.

A step lemma takes a thread other than 0 (`two`'s threads) and says, for every pick, that the
world accepts the step and what the world after is, as a structure of facts (`Loaded`,
`Stored`, `Updated`): which message was read, where the new message went, how the thread's view
grew (by places), and what did not change (other cells, other threads' views). An update reads
a message that is not glued, at or after the pick's place, writes right after it and glues it:
so in a cell with one message not glued (every one written since by an update) it reads the
newest, whatever the pick, and two updates of a cell never read the same message.

Rule R1 is said only as far as a proof shows its own steps are not refused: a write that
releases, or one by a thread with no relaxed read (`refused_of_releases`, `refused_of_loaded`).
-/

namespace Blaze.Threads

open Blaze.core.sync.atomic

/-! ## Programs: `execWeak` and `bind` -/

namespace Prog

@[simp] theorem execWeak_done_ok {α : Type} (v : α) (w : Weak.World) :
    (Prog.done (.ok v)).execWeak w = .ok (v, w) := rfl

@[simp] theorem execWeak_done_fail {α : Type} (e : Error) (w : Weak.World) :
    (Prog.done (.fail e) : Prog α).execWeak w = .fail e := rfl

@[simp] theorem execWeak_done_div {α : Type} (w : Weak.World) :
    (Prog.done .div : Prog α).execWeak w = .div := rfl

/-- One operation of a thread of a run (`who ≠ 0`, which never ends the run by sleeping) that
the world accepts: the program goes on with the answer. -/
theorem execWeak_op {α : Type} {who pick : Nat} {o : Op} {next : Got → Prog α}
    {w w' : Weak.World} {got : Got} (hw : who ≠ 0)
    (h : Weak.World.step who pick o w = some (got, w')) :
    (Prog.op who pick o next).execWeak w = (next got).execWeak w' := by
  simp [execWeak, h, hw]

/-- One operation that does not sleep, by any thread: the program goes on with the answer. -/
theorem execWeak_op_awake {α : Type} {who pick : Nat} {o : Op} {next : Got → Prog α}
    {w w' : Weak.World} {got : Got} (h : Weak.World.step who pick o w = some (got, w'))
    (hs : o.sleeps got = false) :
    (Prog.op who pick o next).execWeak w = (next got).execWeak w' := by
  simp [execWeak, h, hs]

/-- `p >>= f` runs `p`, then `f` on its value in the world after; a panic or running out of
fuel passes through. -/
theorem execWeak_bind {α β : Type} (p : Prog α) (f : α → Prog β) (w : Weak.World) :
    (p >>= f).execWeak w =
      (match p.execWeak w with
       | .ok (v, w') => (f v).execWeak w'
       | .fail e => .fail e
       | .div => .div) := by
  induction p generalizing w with
  | done r => cases r <;> rfl
  | op who pick o next ih =>
    rw [bind_op]
    simp only [execWeak]
    rcases Weak.World.step who pick o w with _ | ⟨got, w'⟩
    · rfl
    · by_cases hc : (who == 0 && o.sleeps got) = true
      · simp only [hc, ↓reduceIte]
      · simp only [hc, Bool.false_eq_true, ↓reduceIte, ih]

/-- A program whose run from the empty weak world ends with `v` has the value `v`. -/
theorem valueWeak_of_execWeak {α : Type} {p : Prog α} {v : α} {w : Weak.World}
    (h : p.execWeak Weak.World.new = .ok (v, w)) : p.valueWeak = .ok v := by
  simp [valueWeak, h]

end Prog

namespace Weak

/-! ## Lists, association lists and places -/

theorem lt_of_get {α : Type} {l : List α} {i : Nat} {a : α} (h : l[i]? = some a) :
    i < l.length := by
  rcases Nat.lt_or_ge i l.length with h' | h'
  · exact h'
  · rw [List.getElem?_eq_none h'] at h; cases h

theorem getAt_putAt_self {β : Type} (l : List (Nat × β)) (k : Nat) (v : β) :
    getAt (putAt l k v) k = some v := by
  induction l with
  | nil => simp [putAt, getAt]
  | cons kv rest ih =>
    obtain ⟨j, u⟩ := kv
    by_cases h : j = k
    · subst h; simp [putAt, getAt]
    · simp [putAt, getAt, h, ih]

theorem getAt_putAt_ne {β : Type} (l : List (Nat × β)) {k k' : Nat} (v : β) (h : k' ≠ k) :
    getAt (putAt l k v) k' = getAt l k' := by
  induction l with
  | nil => simp [putAt, getAt, Ne.symm h]
  | cons kv rest ih =>
    obtain ⟨j, u⟩ := kv
    by_cases hj : j = k
    · subst hj; simp [putAt, getAt, Ne.symm h]
    · simp [putAt, getAt, hj, ih]

theorem getAt_mem {β : Type} {l : List (Nat × β)} {k : Nat} {v : β} (h : getAt l k = some v) :
    (k, v) ∈ l := by
  induction l with
  | nil => simp [getAt] at h
  | cons kv rest ih =>
    obtain ⟨j, u⟩ := kv
    simp only [getAt] at h
    by_cases hj : j = k
    · subst hj; simp at h; subst h; simp
    · simp [hj] at h; exact List.mem_cons_of_mem _ (ih h)

theorem idxOf_lt {msgs : List Msg} {id j : Nat} (h : idxOf msgs id = some j) :
    j < msgs.length := by
  induction msgs generalizing j with
  | nil => simp [idxOf] at h
  | cons m rest ih =>
    simp only [idxOf] at h
    split at h
    · cases h; simp
    · cases hr : idxOf rest id with
      | none => simp [hr] at h
      | some i =>
        simp [hr] at h
        have := ih hr
        simp; omega

/-- A place in a cell that has messages is one of them. -/
theorem placeIn_lt {msgs : List Msg} (x : Option Nat) (h : msgs ≠ []) :
    placeIn msgs x < msgs.length := by
  have hpos : 0 < msgs.length := by
    cases msgs with
    | nil => exact absurd rfl h
    | cons _ _ => simp
  cases x with
  | none => exact hpos
  | some id =>
    show (idxOf msgs id).getD 0 < msgs.length
    cases hi : idxOf msgs id with
    | none => exact hpos
    | some j => exact idxOf_lt hi

theorem le_back (len k floor : Nat) : floor ≤ back len k floor := by
  simp only [back]; split <;> omega

theorem back_lt {len k floor : Nat} (h : floor < len) : back len k floor < len := by
  simp only [back]; split <;> omega

theorem unglued_go {l : List Msg} {p : Nat} (hne : l ≠ [])
    (hl : ∀ m, l[l.length - 1]? = some m → m.glued = false) :
    ∃ j m, unglued.go l p = p + j ∧ l[j]? = some m ∧ m.glued = false := by
  induction l generalizing p with
  | nil => exact absurd rfl hne
  | cons m rest ih =>
    cases hg : m.glued with
    | false => exact ⟨0, m, by simp [unglued.go, hg], by simp, hg⟩
    | true =>
      cases rest with
      | nil => exact absurd (hl m (by simp)) (by simp [hg])
      | cons m2 rest2 =>
        obtain ⟨j, m', hj, hm, hg'⟩ := ih (p := p + 1) (by simp)
          (fun m' h => hl m' (by simpa using h))
        refine ⟨j + 1, m', ?_, by simpa using hm, hg'⟩
        show (if m.glued = true then unglued.go (m2 :: rest2) (p + 1) else p) = p + (j + 1)
        rw [hg, hj]
        simp only [↓reduceIte]
        omega

/-- Where an update or a store goes from place `p` on: the first message there that is not
glued, which exists where the newest is not. -/
theorem unglued_spec {msgs : List Msg} {p : Nat} (hp : p < msgs.length)
    (hl : ∀ m, msgs[msgs.length - 1]? = some m → m.glued = false) :
    ∃ m, p ≤ unglued msgs p ∧ msgs[unglued msgs p]? = some m ∧ m.glued = false := by
  have hne : msgs.drop p ≠ [] := by
    intro h
    have := congrArg List.length h
    simp at this
    omega
  have hl' : ∀ m, (msgs.drop p)[(msgs.drop p).length - 1]? = some m → m.glued = false := by
    intro m h
    rw [List.getElem?_drop, List.length_drop] at h
    apply hl
    rwa [show p + (msgs.length - p - 1) = msgs.length - 1 by omega] at h
  obtain ⟨j, m, hj, hm, hg⟩ := unglued_go (p := p) hne hl'
  refine ⟨m, ?_, ?_, hg⟩
  · show p ≤ unglued.go (msgs.drop p) p
    rw [hj]; omega
  · show msgs[unglued.go (msgs.drop p) p]? = some m
    rw [hj, ← List.getElem?_drop]; exact hm

/-- Where a message is in a cell depends only on its id. -/
theorem idxOf_insertAt {l : List Msg} {n : Nat} {m m' : Msg} (h : m.id = m'.id) (i : Nat) :
    idxOf (insertAt l n m) i = idxOf (insertAt l n m') i := by
  induction l generalizing n with
  | nil => cases n <;> simp [insertAt, idxOf, h]
  | cons x rest ih =>
    cases n with
    | zero => simp [insertAt, idxOf, h]
    | succ n => simp [insertAt, idxOf, ih]

/-- The messages of cell `c`, oldest first; none where there is no such cell. -/
def msgsAt (cells : List (List Msg)) (c : Nat) : List Msg := (cells[c]?).getD []

/-- The place in cell `c` that view `v` names (`placeIn`): 0 where it names none. -/
def placeOf (cells : List (List Msg)) (v : View) (c : Nat) : Nat :=
  placeIn (msgsAt cells c) (getAt v c)

theorem placeOf_putAt_ne {cells : List (List Msg)} {v : View} {k id c : Nat} (h : c ≠ k) :
    placeOf cells (putAt v k id) c = placeOf cells v c := by
  simp only [placeOf, getAt_putAt_ne _ _ h]

theorem placeOf_putAt_self {cells : List (List Msg)} {v : View} {c id : Nat} :
    placeOf cells (putAt v c id) c = (idxOf (msgsAt cells c) id).getD 0 := by
  simp only [placeOf, getAt_putAt_self]; rfl

theorem placeOf_congr {cells cells' : List (List Msg)} {v : View} {c : Nat}
    (h : cells'[c]? = cells[c]?) : placeOf cells' v c = placeOf cells v c := by
  simp only [placeOf, msgsAt, h]

theorem placeIn_congr {l l' : List Msg} (h : ∀ i, idxOf l i = idxOf l' i) (x : Option Nat) :
    placeIn l x = placeIn l' x := by
  cases x <;> simp [placeIn, h]

theorem placeOf_set_congr {cells : List (List Msg)} {k : Nat} {l l' : List Msg}
    (h : ∀ i, idxOf l i = idxOf l' i) (v : View) (c : Nat) :
    placeOf (cells.set k l) v c = placeOf (cells.set k l') v c := by
  unfold placeOf msgsAt
  by_cases hk : k = c
  · subst hk
    by_cases hlt : k < cells.length
    · simp only [List.getElem?_set_self hlt, Option.getD_some]; exact placeIn_congr h _
    · have h1 : (cells.set k l)[k]? = none := List.getElem?_eq_none (by simp; omega)
      have h2 : (cells.set k l')[k]? = none := List.getElem?_eq_none (by simp; omega)
      rw [h1, h2]
  · rw [List.getElem?_set_ne hk, List.getElem?_set_ne hk]

/-- A join keeps what the view had seen. -/
theorem joinView_left (cells : List (List Msg)) (e : View) :
    ∀ (v : View) (c : Nat), placeOf cells v c ≤ placeOf cells (joinView cells v e) c := by
  induction e with
  | nil => intro v c; simp [joinView]
  | cons ce rest ih =>
    obtain ⟨c', id⟩ := ce
    intro v c
    simp only [joinView]
    refine Nat.le_trans ?_ (ih _ c)
    split
    · next hlt =>
      by_cases hc : c = c'
      · subst hc
        rw [placeOf_putAt_self]
        simp only [placeOf, msgsAt]
        omega
      · rw [placeOf_putAt_ne hc]; exact Nat.le_refl _
    · exact Nat.le_refl _

/-- A join sees every message the joined view names. -/
theorem joinView_right (cells : List (List Msg)) (e : View) :
    ∀ (v : View) (c id : Nat), (c, id) ∈ e →
      (idxOf (msgsAt cells c) id).getD 0 ≤ placeOf cells (joinView cells v e) c := by
  induction e with
  | nil => intro v c id h; simp at h
  | cons ce rest ih =>
    obtain ⟨c', id'⟩ := ce
    intro v c id h
    simp only [joinView]
    rcases List.mem_cons.mp h with h | h
    · simp only [Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      refine Nat.le_trans ?_ (joinView_left cells rest _ c)
      split
      · rw [placeOf_putAt_self]; exact Nat.le_refl _
      · next hlt =>
        simp only [placeOf, msgsAt]
        omega
    · exact ih _ c id h

namespace World

/-- The place of thread `t`'s view of cell `c`: no read of `c` by `t` reads an older message. -/
def place (w : World) (t c : Nat) : Nat := placeOf w.cells (w.viewOf t) c

theorem viewOf_self {w w' : World} {who : Nat} {v : View} (h : w'.views = putAt w.views who v) :
    w'.viewOf who = v := by
  simp [viewOf, h, getAt_putAt_self]

theorem viewOf_ne {w w' : World} {who t : Nat} {v : View} (h : w'.views = putAt w.views who v)
    (hw : who ≠ 0) (ht : t ≠ who) : w'.viewOf t = w.viewOf t := by
  simp only [viewOf, h, getAt_putAt_ne _ _ ht, getAt_putAt_ne _ _ (Ne.symm hw)]

/-! ## Rule R1: when a write is not refused -/

theorem refused_of_releases {w : World} {who cell : Nat} {order : Order}
    (h : releases order = true) : w.refused who cell order = false := by
  simp [refused, h]

theorem refused_of_loaded {w : World} {who cell : Nat} {order : Order}
    (h : ∀ tc ∈ w.loaded, tc.1 ≠ who) : w.refused who cell order = false := by
  have : w.loaded.any (fun tc => tc.1 == who && tc.2 != cell) = false := by
    rw [List.any_eq_false]
    intro tc hm
    simp [h tc hm]
  unfold refused
  rw [this, Bool.and_false]

/-! ## A read -/

section read

variable {w : World} {who cell : Nat} {m : Msg} {order : Order}

@[simp] theorem readMsg_cells : (w.readMsg who cell m order).cells = w.cells := rfl

@[simp] theorem readMsg_next : (w.readMsg who cell m order).next = w.next := rfl

theorem readMsg_viewOf : (w.readMsg who cell m order).viewOf who =
    if acquires order then joinView w.cells (putAt (w.viewOf who) cell m.id) m.view
    else putAt (w.viewOf who) cell m.id := viewOf_self rfl

theorem readMsg_viewOf_ne (hw : who ≠ 0) {t : Nat} (ht : t ≠ who) :
    (w.readMsg who cell m order).viewOf t = w.viewOf t := viewOf_ne rfl hw ht

theorem readMsg_place_ne {c : Nat} (hc : c ≠ cell) :
    w.place who c ≤ (w.readMsg who cell m order).place who c := by
  simp only [place, readMsg_viewOf, readMsg_cells]
  split
  · exact Nat.le_trans (Nat.le_of_eq (placeOf_putAt_ne hc).symm) (joinView_left _ _ _ _)
  · exact Nat.le_of_eq (placeOf_putAt_ne hc).symm

theorem readMsg_acquire (ha : acquires order = true) (c : Nat) :
    placeOf w.cells m.view c ≤ (w.readMsg who cell m order).place who c := by
  simp only [place, readMsg_viewOf, readMsg_cells, ha, ↓reduceIte]
  cases hg : getAt m.view c with
  | none => unfold placeOf; rw [hg]; exact Nat.zero_le _
  | some id =>
    have := joinView_right w.cells m.view (putAt (w.viewOf who) cell m.id) c id (getAt_mem hg)
    rw [show placeOf w.cells m.view c = (idxOf (msgsAt w.cells c) id).getD 0 by
      unfold placeOf; rw [hg]; rfl]
    exact this

theorem readMsg_sees :
    (idxOf (msgsAt w.cells cell) m.id).getD 0 ≤ (w.readMsg who cell m order).place who cell := by
  simp only [place, readMsg_viewOf, readMsg_cells]
  split
  · exact Nat.le_trans (Nat.le_of_eq placeOf_putAt_self.symm) (joinView_left _ _ _ _)
  · exact Nat.le_of_eq placeOf_putAt_self.symm

theorem readMsg_relaxed (ha : acquires order = false) :
    getAt ((w.readMsg who cell m order).viewOf who) cell = some m.id := by
  rw [readMsg_viewOf, ha]
  simp [getAt_putAt_self]

theorem readMsg_loaded :
    ∀ tc ∈ (w.readMsg who cell m order).loaded, tc ∈ w.loaded ∨ tc = (who, cell) := by
  intro tc h
  simp only [readMsg] at h
  split at h
  · exact .inl h
  · rcases List.mem_cons.mp h with h | h
    · exact .inr h
    · exact .inl h

end read

/-! ## A write -/

/-- The view a write's message carries (`writeMsg`). -/
def wview (w : World) (who cell : Nat) (msgs : List Msg) (at_ : Nat) (value : Int)
    (order : Order) (extra : View) : View :=
  joinView (w.cells.set cell (insertAt msgs at_ ⟨w.next, value, [], false⟩))
    (if releases order then putAt (w.viewOf who) cell w.next else [(cell, w.next)]) extra

/-- The message a write puts (`writeMsg`). -/
def wmsg (w : World) (who cell : Nat) (msgs : List Msg) (at_ : Nat) (value : Int)
    (order : Order) (extra : View) : Msg :=
  ⟨w.next, value, w.wview who cell msgs at_ value order extra, false⟩

section write

variable {w : World} {who cell : Nat} {msgs : List Msg} {at_ : Nat} {value : Int}
  {order : Order} {extra : View}

theorem writeMsg_cells : (w.writeMsg who cell msgs at_ value order extra).cells =
    w.cells.set cell (insertAt msgs at_ (w.wmsg who cell msgs at_ value order extra)) := rfl

theorem writeMsg_views : (w.writeMsg who cell msgs at_ value order extra).views =
    putAt w.views who (putAt (w.viewOf who) cell w.next) := rfl

@[simp] theorem writeMsg_next : (w.writeMsg who cell msgs at_ value order extra).next =
    w.next + 1 := rfl

@[simp] theorem writeMsg_loaded : (w.writeMsg who cell msgs at_ value order extra).loaded =
    w.loaded := rfl

theorem writeMsg_get (hc : cell < w.cells.length) :
    (w.writeMsg who cell msgs at_ value order extra).cells[cell]? =
      some (insertAt msgs at_ (w.wmsg who cell msgs at_ value order extra)) := by
  rw [writeMsg_cells, List.getElem?_set_self hc]

theorem writeMsg_get_ne {c : Nat} (hc : c ≠ cell) :
    (w.writeMsg who cell msgs at_ value order extra).cells[c]? = w.cells[c]? := by
  rw [writeMsg_cells, List.getElem?_set_ne (Ne.symm hc)]

theorem writeMsg_viewOf : (w.writeMsg who cell msgs at_ value order extra).viewOf who =
    putAt (w.viewOf who) cell w.next := viewOf_self writeMsg_views

theorem writeMsg_viewOf_ne (hw : who ≠ 0) {t : Nat} (ht : t ≠ who) :
    (w.writeMsg who cell msgs at_ value order extra).viewOf t = w.viewOf t :=
  viewOf_ne writeMsg_views hw ht

theorem writeMsg_place_ne {c : Nat} (hc : c ≠ cell) :
    (w.writeMsg who cell msgs at_ value order extra).place who c = w.place who c := by
  simp only [place, writeMsg_viewOf, placeOf_congr (writeMsg_get_ne hc), placeOf_putAt_ne hc]

/-- The cells a write's message view was joined over, and the cells after, name the same
places: they differ only in that message's view. -/
theorem writeMsg_placed (v : View) (c : Nat) :
    placeOf (w.cells.set cell (insertAt msgs at_ ⟨w.next, value, [], false⟩)) v c =
      placeOf (w.writeMsg who cell msgs at_ value order extra).cells v c := by
  rw [writeMsg_cells]
  exact placeOf_set_congr (idxOf_insertAt (m := ⟨w.next, value, [], false⟩)
    (m' := w.wmsg who cell msgs at_ value order extra) rfl) v c

theorem writeMsg_release (hr : releases order = true) (c : Nat) :
    (w.writeMsg who cell msgs at_ value order extra).place who c ≤
      placeOf (w.writeMsg who cell msgs at_ value order extra).cells
        (w.wmsg who cell msgs at_ value order extra).view c := by
  have hv : (w.wmsg who cell msgs at_ value order extra).view =
      joinView (w.cells.set cell (insertAt msgs at_ ⟨w.next, value, [], false⟩))
        (putAt (w.viewOf who) cell w.next) extra := by
    simp [wmsg, wview, hr]
  simp only [place, writeMsg_viewOf, hv]
  rw [← writeMsg_placed, ← writeMsg_placed]
  exact joinView_left _ _ _ _

theorem writeMsg_extra {c : Nat} (hc : c ≠ cell) :
    placeOf w.cells extra c ≤
      placeOf (w.writeMsg who cell msgs at_ value order extra).cells
        (w.wmsg who cell msgs at_ value order extra).view c := by
  cases hg : getAt extra c with
  | none => unfold placeOf; rw [hg]; exact Nat.zero_le _
  | some id =>
    have h1 := joinView_right (w.cells.set cell (insertAt msgs at_ ⟨w.next, value, [], false⟩))
      extra (if releases order then putAt (w.viewOf who) cell w.next else [(cell, w.next)])
      c id (getAt_mem hg)
    have h2 : msgsAt (w.cells.set cell (insertAt msgs at_ ⟨w.next, value, [], false⟩)) c =
        msgsAt w.cells c := by
      simp only [msgsAt, List.getElem?_set_ne (Ne.symm hc)]
    rw [h2] at h1
    rw [show placeOf w.cells extra c = (idxOf (msgsAt w.cells c) id).getD 0 by
      unfold placeOf; rw [hg]; rfl, ← writeMsg_placed]
    exact h1

end write

/-! ## One step of a thread of a run, for every pick -/

/-- What a load of `cell` by `who` did, from `w` to `w'`: it read `m`, at place `i` of the
cell's messages `msgs`. -/
structure Loaded (w w' : World) (who cell : Nat) (order : Order) (msgs : List Msg) (i : Nat)
    (m : Msg) : Prop where
  read : msgs[i]? = some m
  /-- It is not older than the thread's view of the cell. -/
  not_older : w.place who cell ≤ i
  /-- The thread's view of the cell is at least that message... -/
  sees : (idxOf msgs m.id).getD 0 ≤ w'.place who cell
  /-- ...and, where the load is relaxed, exactly it. -/
  relaxed : acquires order = false → getAt (w'.viewOf who) cell = some m.id
  /-- Where it acquires, the thread's view is joined with the message's. -/
  acquire : acquires order = true → ∀ c, placeOf w.cells m.view c ≤ w'.place who c
  cells_eq : w'.cells = w.cells
  next_eq : w'.next = w.next
  views_ne : ∀ t, t ≠ who → w'.viewOf t = w.viewOf t
  place_ne : ∀ c, c ≠ cell → w.place who c ≤ w'.place who c
  loaded_sub : ∀ tc ∈ w'.loaded, tc ∈ w.loaded ∨ tc = (who, cell)

/-- A load by a thread of a run, whatever the pick: it reads a message of the cell not older
than the thread's view of it. -/
theorem step_load {w : World} {who pick cell : Nat} {order : Order} {msgs : List Msg}
    (hw : who ≠ 0) (hc : w.cells[cell]? = some msgs) (hne : msgs ≠ []) :
    ∃ got w' i m, w.step who pick (.load cell order) = some (got, w') ∧
      got = ⟨m.value, true⟩ ∧ Loaded w w' who cell order msgs i m := by
  have hmsgs : msgsAt w.cells cell = msgs := by simp [msgsAt, hc]
  have hpl : w.place who cell = placeIn msgs (getAt (w.viewOf who) cell) := by
    simp only [place, placeOf, hmsgs]
  have hp := back_lt (k := pick) (placeIn_lt (getAt (w.viewOf who) cell) hne)
  obtain ⟨m, hm⟩ : ∃ m,
      msgs[back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))]? = some m :=
    ⟨_, List.getElem?_eq_getElem hp⟩
  have h0 : (who == 0) = false := by simp [hw]
  have hs : w.step who pick (.load cell order) =
      some (⟨m.value, true⟩, w.readMsg who cell m order) := by
    simp only [step, h0, hc, hm, Bool.false_eq_true, ↓reduceIte]
  refine ⟨_, _, _, m, hs, rfl, hm, ?_, ?_, readMsg_relaxed, readMsg_acquire, rfl, rfl,
    fun t ht => readMsg_viewOf_ne hw ht, fun c hc' => readMsg_place_ne hc', readMsg_loaded⟩
  · rw [hpl]; exact le_back ..
  · rw [← hmsgs]; exact readMsg_sees

/-- What a store of `v` to `cell` by `who` did, from `w` to `w'`: it put `m` right after place
`q` of the cell's messages `msgs`. -/
structure Stored (w w' : World) (who cell : Nat) (v : Int) (order : Order) (msgs : List Msg)
    (q : Nat) (m : Msg) : Prop where
  /-- After the thread's view of the cell. -/
  after : w.place who cell ≤ q
  lt : q < msgs.length
  put : w'.cells[cell]? = some (insertAt msgs (q + 1) m)
  id_eq : m.id = w.next
  value_eq : m.value = v
  unglued : m.glued = false
  /-- The thread's view of the cell is the new message. -/
  sees : getAt (w'.viewOf who) cell = some m.id
  /-- Where the store releases, the message's view is the thread's. -/
  release : releases order = true → m.view = w'.viewOf who
  next_eq : w'.next = w.next + 1
  cells_ne : ∀ c, c ≠ cell → w'.cells[c]? = w.cells[c]?
  views_ne : ∀ t, t ≠ who → w'.viewOf t = w.viewOf t
  place_ne : ∀ c, c ≠ cell → w'.place who c = w.place who c
  loaded_eq : w'.loaded = w.loaded

/-- A store by a thread of a run that rule R1 does not refuse, whatever the pick, to a cell
whose newest message is not glued. -/
theorem step_store {w : World} {who pick cell : Nat} {v : Int} {order : Order}
    {msgs : List Msg} (hw : who ≠ 0) (hc : w.cells[cell]? = some msgs) (hne : msgs ≠ [])
    (hl : ∀ m, msgs[msgs.length - 1]? = some m → m.glued = false)
    (hr : w.refused who cell order = false) :
    ∃ got w' q m, w.step who pick (.store cell v order) = some (got, w') ∧
      Stored w w' who cell v order msgs q m := by
  have hmsgs : msgsAt w.cells cell = msgs := by simp [msgsAt, hc]
  have hpl : w.place who cell = placeIn msgs (getAt (w.viewOf who) cell) := by
    simp only [place, placeOf, hmsgs]
  have hp := back_lt (k := pick) (placeIn_lt (getAt (w.viewOf who) cell) hne)
  obtain ⟨mq, hle, hmq, -⟩ := unglued_spec hp hl
  have h0 : (who == 0) = false := by simp [hw]
  have hcl := lt_of_get hc
  have hs : w.step who pick (.store cell v order) =
      some (⟨0, true⟩, w.writeMsg who cell msgs
        (unglued msgs (back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))) + 1)
        v order []) := by
    simp only [step, h0, hc, hr, Bool.false_eq_true, ↓reduceIte]
  generalize unglued msgs (back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))) = q
    at hle hmq hs
  refine ⟨_, _, q, w.wmsg who cell msgs (q + 1) v order [], hs, ?_, lt_of_get hmq,
    writeMsg_get hcl, rfl, rfl, rfl, ?_, ?_, rfl, fun c hc' => writeMsg_get_ne hc',
    fun t ht => writeMsg_viewOf_ne hw ht, fun c hc' => writeMsg_place_ne hc', rfl⟩
  · rw [hpl]; exact Nat.le_trans (le_back ..) hle
  · rw [writeMsg_viewOf, getAt_putAt_self]; rfl
  · intro hr'
    rw [writeMsg_viewOf]
    simp [wmsg, wview, hr', joinView]

/-- What an update of `cell` by `who` that wrote did, from `w` to `w'`: it read `m`, at place
`q` of the cell's messages `msgs`, glued it and put `m'` right after it. -/
structure Updated (w w' : World) (who cell : Nat) (change : Int → Option Int) (s : Order)
    (msgs : List Msg) (q : Nat) (m m' : Msg) : Prop where
  /-- Not older than the thread's view of the cell. -/
  not_older : w.place who cell ≤ q
  read : msgs[q]? = some m
  /-- What it read was not glued: no other update read it. -/
  unglued : m.glued = false
  /-- It glued what it read and wrote right after it. -/
  put : w'.cells[cell]? = some (insertAt (msgs.set q { m with glued := true }) (q + 1) m')
  change_eq : change m.value = some m'.value
  id_eq : m'.id = w.next
  unglued' : m'.glued = false
  /-- The thread's view of the cell is the new message. -/
  sees : getAt (w'.viewOf who) cell = some m'.id
  /-- Where it acquires, the thread's view of the other cells is joined with the read
  message's. -/
  acquire : acquires s = true → ∀ c, c ≠ cell → placeOf w.cells m.view c ≤ w'.place who c
  /-- Where it releases, the new message's view is at least the thread's after. -/
  release : releases s = true → ∀ c, w'.place who c ≤ placeOf w'.cells m'.view c
  /-- Whatever the ordering, the new message carries the read message's view (a release
  sequence). -/
  sequence : ∀ c, c ≠ cell → placeOf w.cells m.view c ≤ placeOf w'.cells m'.view c
  next_eq : w'.next = w.next + 1
  cells_ne : ∀ c, c ≠ cell → w'.cells[c]? = w.cells[c]?
  views_ne : ∀ t, t ≠ who → w'.viewOf t = w.viewOf t
  place_ne : ∀ c, c ≠ cell → w.place who c ≤ w'.place who c
  loaded_sub : ∀ tc ∈ w'.loaded, tc ∈ w.loaded ∨ tc = (who, cell)

/-- A strong update that always writes (a swap, a `fetch_add`) by a thread of a run that rule
R1 does not refuse, whatever the pick, of a cell whose newest message is not glued. -/
theorem step_update {w : World} {who pick cell : Nat} {change : Int → Option Int}
    {s f : Order} {msgs : List Msg} (hw : who ≠ 0) (hc : w.cells[cell]? = some msgs)
    (hne : msgs ≠ []) (hl : ∀ m, msgs[msgs.length - 1]? = some m → m.glued = false)
    (hch : ∀ x, (change x).isSome = true) (hr : w.refused who cell s = false) :
    ∃ got w' q m m', w.step who pick (.update cell change false s f) = some (got, w') ∧
      got = ⟨m.value, true⟩ ∧ Updated w w' who cell change s msgs q m m' := by
  have hmsgs : msgsAt w.cells cell = msgs := by simp [msgsAt, hc]
  have hpl : w.place who cell = placeIn msgs (getAt (w.viewOf who) cell) := by
    simp only [place, placeOf, hmsgs]
  have hp := back_lt (k := pick) (placeIn_lt (getAt (w.viewOf who) cell) hne)
  obtain ⟨m0, hm0⟩ : ∃ m0,
      msgs[back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))]? = some m0 :=
    ⟨_, List.getElem?_eq_getElem hp⟩
  obtain ⟨m, hle, hmq, hg⟩ := unglued_spec hp hl
  obtain ⟨n0, hn0⟩ := Option.isSome_iff_exists.mp (hch m0.value)
  obtain ⟨n, hn⟩ := Option.isSome_iff_exists.mp (hch m.value)
  have h0 : (who == 0) = false := by simp [hw]
  have hcl := lt_of_get hc
  have hs : w.step who pick (.update cell change false s f) =
      some (⟨m.value, true⟩, (w.readMsg who cell m s).writeMsg who cell
        (msgs.set (unglued msgs (back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))))
          { m with glued := true })
        (unglued msgs (back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))) + 1)
        n s m.view) := by
    simp only [step, h0, hc, hm0, hn0, hmq, hn, hr, Bool.false_eq_true, ↓reduceIte]
  generalize unglued msgs (back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))) = q
    at hle hmq hs
  have hcl' : cell < (w.readMsg who cell m s).cells.length := hcl
  refine ⟨_, _, q, m, (w.readMsg who cell m s).wmsg who cell (msgs.set q { m with glued := true })
    (q + 1) n s m.view, hs, rfl, ?_, hmq, hg, writeMsg_get hcl', hn, rfl, rfl, ?_, ?_,
    fun hr' c => writeMsg_release (w := w.readMsg who cell m s) hr' c,
    fun c hc' => writeMsg_extra (w := w.readMsg who cell m s) hc', rfl,
    fun c hc' => writeMsg_get_ne hc', ?_, ?_, fun tc h => readMsg_loaded (m := m) (order := s) tc h⟩
  · rw [hpl]; exact Nat.le_trans (le_back ..) hle
  · rw [writeMsg_viewOf, getAt_putAt_self]; rfl
  · intro ha c hc'
    rw [writeMsg_place_ne hc']
    exact readMsg_acquire ha c
  · intro t ht
    rw [writeMsg_viewOf_ne hw ht, readMsg_viewOf_ne hw ht]
  · intro c hc'
    rw [writeMsg_place_ne hc']
    exact readMsg_place_ne hc'

end World

end Weak

/-! ## Two threads -/

/-- `Advance` in the weak world: what one step of thread `who`, with the world's `pick`, from
`p` in `w` must do for `P` to hold where `two` goes on. -/
def AdvanceWeak {α : Type} (who pick : Nat) (p : Prog α) (w : Weak.World)
    (P : Prog α → Weak.World → Prop) : Prop :=
  match p with
  | .done r => P (.done r) w
  | .op tagged k o next =>
    tagged = 0 ∧ ∃ got w', Weak.World.step who pick o w = some (got, w') ∧
      P (if o.sleeps got then .op tagged k o next else next got) w'

/-- A step from a point whose shape is `hp`, by the world's step `hs`. -/
theorem AdvanceWeak.of_step {α : Type} {who pick k : Nat} {p : Prog α} {o : Op}
    {next : Got → Prog α} {w w' : Weak.World} {got : Got} {P : Prog α → Weak.World → Prop}
    (hp : p = .op 0 k o next) (hs : Weak.World.step who pick o w = some (got, w'))
    (hP : P (if o.sleeps got then p else next got) w') : AdvanceWeak who pick p w P := by
  subst hp; exact ⟨rfl, got, w', hs, hP⟩

/-- A step of an operation that does not sleep (a load, a store, an update). -/
theorem AdvanceWeak.of_awake {α : Type} {who pick k : Nat} {p : Prog α} {o : Op}
    {next : Got → Prog α} {w w' : Weak.World} {got : Got} {P : Prog α → Weak.World → Prop}
    (hp : p = .op 0 k o next) (hs : Weak.World.step who pick o w = some (got, w'))
    (ho : o.sleeps got = false) (hP : P (next got) w') : AdvanceWeak who pick p w P := by
  subst hp; exact ⟨rfl, got, w', hs, by rw [ho]; exact hP⟩

/-- A thread that has returned. -/
theorem AdvanceWeak.of_done {α : Type} {who pick : Nat} {p : Prog α} {r : Result α}
    {w : Weak.World} {P : Prog α → Weak.World → Prop} (hp : p = .done r) (hP : P p w) :
    AdvanceWeak who pick p w P := by
  subst hp; exact hP

/-- `two_exec` in the weak world: an invariant over the steps left, the two threads' programs
and the world that each thread's step keeps holds when the schedule ends, and the run is the
run of the end from there. -/
theorem two_exec_weak {α β : Type} (Inv : Nat → Prog α → Prog β → Weak.World → Prop)
    (first : ∀ n a b w pick, Inv (n + 1) a b w →
      AdvanceWeak 1 pick a w (fun a' w' => Inv n a' b w'))
    (second : ∀ n a b w pick, Inv (n + 1) a b w →
      AdvanceWeak 2 pick b w (fun b' w' => Inv n a b' w')) :
    ∀ (schedule : List Step) (a : Prog α) (b : Prog β) (w : Weak.World),
      Inv schedule.length a b w →
      ∃ a' b' w', Inv 0 a' b' w' ∧ (two schedule a b).execWeak w = (two [] a' b').execWeak w' := by
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
        simp only [turn, bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
        rw [Prog.execWeak_op (by decide) hs]
        exact he
    · have ha := first rest.length a b w step.pick.val.toNat h
      cases a with
      | done r => exact ih (.done r) b w ha
      | op tg k o next =>
        obtain ⟨rfl, got, w', hs, hP⟩ := ha
        obtain ⟨a', b', w'', hI, he⟩ := ih _ _ _ hP
        refine ⟨a', b', w'', hI, ?_⟩
        simp only [turn, bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
        rw [Prog.execWeak_op (by decide) hs]
        exact he

/-- `two_ends` in the weak world: for every schedule and every pick, with `Inv` kept by each
thread's step and every end in `Inv 0` satisfying `Post`, the run of `two schedule a b` from
`Inv` ends, and how each thread stood and the world satisfy `Post`. -/
theorem two_ends_weak {α β : Type} (Inv : Nat → Prog α → Prog β → Weak.World → Prop)
    (first : ∀ n a b w pick, Inv (n + 1) a b w →
      AdvanceWeak 1 pick a w (fun a' w' => Inv n a' b w'))
    (second : ∀ n a b w pick, Inv (n + 1) a b w →
      AdvanceWeak 2 pick b w (fun b' w' => Inv n a b' w'))
    (Post : Ended α → Ended β → Weak.World → Prop)
    (last : ∀ a b w, Inv 0 a b w →
      ∃ ea eb w', (two [] a b).execWeak w = .ok ((ea, eb), w') ∧ Post ea eb w')
    (schedule : List Step) (a : Prog α) (b : Prog β) (w : Weak.World)
    (h : Inv schedule.length a b w) :
    ∃ ea eb w', (two schedule a b).execWeak w = .ok ((ea, eb), w') ∧ Post ea eb w' := by
  obtain ⟨a', b', w', hI, he⟩ := two_exec_weak Inv first second schedule a b w h
  obtain ⟨ea, eb, w'', he', hP⟩ := last a' b' w' hI
  exact ⟨ea, eb, w'', he.trans he', hP⟩

/-- The end of a schedule in the weak world: how thread 1 stands, then thread 2. -/
theorem two_nil_ok_weak {α β : Type} {a : Prog α} {b : Prog β} {w w1 w2 : Weak.World}
    {ea : Ended α} {eb : Ended β} (ha : (ended 1 a).execWeak w = .ok (ea, w1))
    (hb : (ended 2 b).execWeak w1 = .ok (eb, w2)) :
    (two [] a b).execWeak w = .ok ((ea, eb), w2) := by
  simp only [two, Prog.execWeak_bind, ha, hb]
  rfl

/-- A thread that returned. -/
@[simp] theorem ended_execWeak_done {α : Type} (who : Nat) (v : α) (w : Weak.World) :
    (ended who (.done (.ok v))).execWeak w = .ok (.Done v, w) := rfl

/-- A thread at an operation that is not a wait can go on. -/
theorem ended_execWeak_running {α : Type} {who t k : Nat} {o : Op} {next : Got → Prog α}
    (w : Weak.World) (ho : ∀ c e tm, o ≠ .wait c e tm) (hu : ∀ c e dl, o ≠ .wait_until c e dl) :
    (ended who (.op t k o next)).execWeak w = .ok (.Running, w) := by
  cases o with
  | wait c e tm => exact absurd rfl (ho c e tm)
  | wait_until c e dl => exact absurd rfl (hu c e dl)
  | _ => rfl

end Blaze.Threads
