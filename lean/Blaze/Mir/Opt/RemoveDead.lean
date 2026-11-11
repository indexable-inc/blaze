import Blaze.Mir.Opt.Liveness
import Blaze.Mir.Opt.PutsTerm

/-!
Global dead-store elimination refines the program: ledger V11, pieces (d3) and (e).

The relation (`Rel`). The source's state is the target's with a list of slots replaced
(`puts t reps`), and of the target's state: the borrow invariant holds (`Borrows`); the
frames' ids strictly decrease down the stack and each frame's destination is in a frame
made before it (`Ordered`); and each entry is `Good`: its slot holds a value the invariant
allows, its local is one its frame's function never borrows, every frame's destination is
its whole slot or off it, and it is dead where its frame resumes (`Placed`): for the running
frame, on entry to its block; for a frame under it, on entry to the block its callee returns
to, unless the callee's destination is the entry's whole slot. An entry of a frame that
returned stays in the list and replaces nothing.

Inside a block the running frame's entries are dead at the current point of the certificate
(`Here`). `block_rel`: by induction on the statements with their points, a kept statement
carries or absorbs each entry (`execStmt_absorb`, by `stmtLive_dead`), a removed store adds
one (`dead_store_src`), and the terminator keeps the relation (`execTerm_rel`: `jump_rel`,
`ret_rel`, `callOn_rel`).
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-! Outcomes of related steps, through `bind`. -/

theorem StepRel.of_map (R : State → State → Prop) (F : State → State) :
    ∀ (x : Outcome Next), (∀ (t' : State), x = Outcome.ok (Next.state t') → R (F t') t') →
      StepRel R (Outcome.map (Next.map F) x) x
  | Outcome.ok (Next.state t'), h => StepRel.state (F t') t' (h t' rfl)
  | Outcome.ok (Next.done v), _ => StepRel.done v
  | Outcome.panic e, _ => StepRel.panic e
  | Outcome.undefined why, _ => StepRel.undefined why
  | Outcome.diverge, _ => StepRel.diverge

theorem StepRel.bind_map {α α' : Type} (R : State → State → Prop) {s : α → α'} {x : Outcome α}
    {x' : Outcome α'} {f : α' → Outcome Next} {g : α → Outcome Next}
    (hx : x' = Outcome.map s x)
    (h : ∀ (a : α), x = Outcome.ok a → StepRel R (f (s a)) (g a)) :
    StepRel R (x'.bind f) (x.bind g) := by
  subst hx
  cases x with
  | ok a => exact h a rfl
  | panic e => exact StepRel.panic e
  | undefined why => exact StepRel.undefined why
  | diverge => exact StepRel.diverge

theorem StepRel.bind_same {α : Type} (R : State → State → Prop) {x x' : Outcome α}
    {f g : α → Outcome Next} (hx : x' = x)
    (h : ∀ (a : α), x = Outcome.ok a → StepRel R (f a) (g a)) :
    StepRel R (x'.bind f) (x.bind g) := by
  rw [hx]
  cases x with
  | ok a => exact h a rfl
  | panic e => exact StepRel.panic e
  | undefined why => exact StepRel.undefined why
  | diverge => exact StepRel.diverge

/-- The same where the source may also be undefined. -/
theorem StepRel.bind_map_or {α α' : Type} (R : State → State → Prop) {s : α → α'}
    {x : Outcome α} {x' : Outcome α'} {f : α' → Outcome Next} {g : α → Outcome Next}
    (hx : x' = Outcome.map s x)
    (h : ∀ (a : α), x = Outcome.ok a →
      (∃ why, f (s a) = Outcome.undefined why) ∨ StepRel R (f (s a)) (g a)) :
    (∃ why, x'.bind f = Outcome.undefined why) ∨ StepRel R (x'.bind f) (x.bind g) := by
  subst hx
  cases x with
  | ok a => exact h a rfl
  | panic e => exact Or.inr (StepRel.panic e)
  | undefined why => exact Or.inr (StepRel.undefined why)
  | diverge => exact Or.inr StepRel.diverge

/-! Small facts. -/

theorem and_true_intro {a b : Bool} (ha : a = true) (hb : b = true) : (a && b) = true := by
  subst ha
  subst hb
  rfl

theorem all_mem {α : Type} (p : α → Bool) :
    ∀ (xs : List α), xs.all p = true → ∀ (x : α), x ∈ xs → p x = true
  | [], _, _, m => mem_nil_elim m
  | a :: rest, h, x, m => by
    have h' : (p a && rest.all p) = true := h
    cases List.mem_cons.mp m with
    | inl e =>
      rw [e]
      exact and_true_left h'
    | inr m' => exact all_mem p rest (and_true_right h') x m'

theorem mem_keepReps (top : Nat) (s : Statement) :
    ∀ (reps : List Rep) (r : Rep), r ∈ keepReps top s reps →
      r ∈ reps ∧ ¬ absorbs top s r = true
  | [], _, m => mem_nil_elim m
  | x :: rest, r, m => by
    by_cases ha : absorbs top s x = true
    · have kept : keepReps top s (x :: rest) = keepReps top s rest :=
        (keepReps_cons top s x rest).trans (if_pos ha)
      have m' : r ∈ keepReps top s rest := by
        rw [kept] at m
        exact m
      have ih := mem_keepReps top s rest r m'
      exact And.intro (List.mem_cons.mpr (Or.inr (And.left ih))) (And.right ih)
    · have kept : keepReps top s (x :: rest) = x :: keepReps top s rest :=
        (keepReps_cons top s x rest).trans (if_neg ha)
      have m' : r ∈ x :: keepReps top s rest := by
        rw [kept] at m
        exact m
      cases List.mem_cons.mp m' with
      | inl here =>
        rw [here]
        exact And.intro (List.mem_cons.mpr (Or.inl rfl)) ha
      | inr there =>
        have ih := mem_keepReps top s rest r there
        exact And.intro (List.mem_cons.mpr (Or.inr (And.left ih))) (And.right ih)

theorem wholeDest_eq (l : Local) :
    ∀ (dest : Place), wholeDest l dest = true → dest = Place.mk l []
  | { base := base, proj := proj }, h => by
    have hb : base = l := of_decide_eq_true (and_true_left h)
    have hp : proj = [] := of_decide_eq_true (and_true_right h)
    rw [hb, hp]

theorem optLive_of (P : BlockId → Prop) :
    ∀ (target : Option BlockId), (∀ (x : BlockId), x ∈ optList target → P x) → OptLive P target
  | none, _ => True.intro
  | some tb, h => h tb (List.mem_cons.mpr (Or.inl rfl))

theorem switchTarget_mem (key : Int) (otherwise : BlockId) :
    ∀ (arms : List (Int × BlockId)),
      switchTarget key arms otherwise ∈ arms.map (fun arm => arm.2) ++ [otherwise]
  | [] => List.mem_cons.mpr (Or.inl rfl)
  | arm :: rest => by
    by_cases h : key = arm.1
    · have e : switchTarget key (arm :: rest) otherwise = arm.2 :=
        (switchTarget_cons key arm rest otherwise).trans (if_pos h)
      rw [e]
      exact List.mem_cons.mpr (Or.inl rfl)
    · have e : switchTarget key (arm :: rest) otherwise = switchTarget key rest otherwise :=
        (switchTarget_cons key arm rest otherwise).trans (if_neg h)
      rw [e]
      exact List.mem_cons.mpr (Or.inr (switchTarget_mem key otherwise rest))

/-! Where a place can be. -/

theorem projStep_frame {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (hm : MemOk (PtrOk B g) st) (p : Pointer) :
    ∀ (e : ProjElem) (q : Pointer), projStep st p e = Outcome.ok q →
      q.frame = p.frame ∨ PtrOk B g q
  | ProjElem.deref, q, h => Or.inr (projStep_deref hm p q h)
  | ProjElem.field i, q, h => by
    have e : p.push (PathElem.field i) = q := Outcome.ok.inj h
    exact Or.inl (congrTo Pointer.frame e.symm)
  | ProjElem.downcast variant, q, h => by
    have e : p.push (PathElem.downcast variant) = q := Outcome.ok.inj h
    exact Or.inl (congrTo Pointer.frame e.symm)
  | ProjElem.index i, q, h => by
    have h' : (st.read { frame := st.top, slot := i, path := [] }).bind (fun x =>
        (st.read p).bind (fun arr => indexBy p arr x)) = Outcome.ok q := h
    have ⟨x, _, r⟩ := Outcome.bind_eq_ok _ _ _ h'
    have r' : (st.read p).bind (fun arr => indexBy p arr x) = Outcome.ok q := r
    have ⟨arr, _, r2⟩ := Outcome.bind_eq_ok _ _ _ r'
    exact Or.inl (And.left (indexBy_same p arr x q r2))
  | ProjElem.constIndex n, q, h => by
    have h' : (st.read p).bind (indexAt p n) = Outcome.ok q := h
    have ⟨arr, _, r⟩ := Outcome.bind_eq_ok _ _ _ h'
    exact Or.inl (And.left (indexAt_same p n arr q r))

/-- A projection ends in the frame it started in, or at a pointer the invariant allows. -/
theorem evalProj_frame {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (hm : MemOk (PtrOk B g) st) :
    ∀ (proj : List ProjElem) (p q : Pointer), evalProj st p proj = Outcome.ok q →
      q.frame = p.frame ∨ PtrOk B g q
  | [], p, q, h => by
    have e : p = q := Outcome.ok.inj h
    exact Or.inl (congrTo Pointer.frame e.symm)
  | e :: rest, p, q, h => by
    have ⟨p1, h1, r, _⟩ := evalProj_step hm e rest p q h
    cases evalProj_frame hm rest p1 q r with
    | inr ok => exact Or.inr ok
    | inl same =>
      cases projStep_frame hm p e p1 h1 with
      | inl s1 => exact Or.inl (same.trans s1)
      | inr ok1 => exact Or.inr (evalProj_ptr hm rest p1 q (Or.inl ok1) r)

/-- A pointer the invariant allows is into a frame already made. -/
theorem ptrOk_lt {B : FnId → Local → Prop} {g : List (Nat × FnId)} {n : Nat}
    (hk : KeysBelow g n) {q : Pointer} (h : PtrOk B g q) : q.frame < n := by
  have h' : q.frame = 0 ∨ FrameHas B q.slot (fnOfIds g q.frame) := h
  cases h' with
  | inl zero =>
    have pos : 0 < n := And.left hk
    omega
  | inr hh =>
    cases hl : fnOfIds g q.frame with
    | none =>
      rw [hl] at hh
      exact False.elim hh
    | some f =>
      have ⟨e, he, hkey⟩ := fnOfIds_key g q.frame f hl
      have lt : e.1 < n := And.right (And.right hk e he)
      omega

/-! The relation. -/

/-- The frames' ids strictly decrease down the stack, and each frame's destination is in a
frame with a smaller id. -/
def Ordered : List Frame → Prop
  | [] => True
  | fr :: rest => fr.dest.frame < fr.id ∧ (∀ (x : Frame), x ∈ rest → x.id < fr.id) ∧ Ordered rest

theorem Ordered.own :
    ∀ (sk : List Frame), Ordered sk → ∀ (c : Frame), c ∈ sk → c.dest.frame < c.id
  | [], _, _, m => mem_nil_elim m
  | fr :: rest, h, c, m => by
    have h' : fr.dest.frame < fr.id ∧ (∀ (x : Frame), x ∈ rest → x.id < fr.id) ∧ Ordered rest :=
      h
    cases List.mem_cons.mp m with
    | inl e =>
      rw [e]
      exact And.left h'
    | inr m' => exact Ordered.own rest (And.right (And.right h')) c m'

/-- Every frame's destination is in a frame under the running one. -/
theorem Ordered.dest_lt (fr : Frame) (rest : List Frame) (h : Ordered (fr :: rest)) (c : Frame)
    (m : c ∈ fr :: rest) : c.dest.frame < fr.id := by
  have h' : fr.dest.frame < fr.id ∧ (∀ (x : Frame), x ∈ rest → x.id < fr.id) ∧ Ordered rest := h
  have own : c.dest.frame < c.id := Ordered.own (fr :: rest) h c m
  cases List.mem_cons.mp m with
  | inl e =>
    rw [e] at own
    exact own
  | inr m' =>
    have lt : c.id < fr.id := And.left (And.right h') c m'
    omega

theorem ordered_withBlock (fr : Frame) (rest : List Frame) (x : BlockId)
    (h : Ordered (fr :: rest)) : Ordered (fr.withBlock x :: rest) := by
  have h' : fr.dest.frame < fr.id ∧ (∀ (x : Frame), x ∈ rest → x.id < fr.id) ∧ Ordered rest := h
  exact And.intro (And.left h') (And.right h')

/-- The entry is dead where the frames under a frame resume. `above` is the destination and
the return target of the frame over the first of `sk`: the entry's frame, when it is the
first, resumes at that target after that destination is written. -/
def Below (D : FnId → BlockId → Local → Prop) (r : Rep) :
    Pointer × Option BlockId → List Frame → Prop
  | _, [] => True
  | above, fr :: rest =>
    (fr.id = r.1 ∧ (atPtr above.1 r = true ∨ OptLive (fun tb => D fr.fn tb r.2.1) above.2)) ∨
      (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest)

/-- The entry is dead where its frame resumes: `P` of the running frame, when it is the
running frame's; else as `Below`. An entry of no frame of the stack is allowed: its frame
returned. -/
def Placed (D : FnId → BlockId → Local → Prop) (P : Frame → Local → Prop) (r : Rep) :
    List Frame → Prop
  | [] => True
  | fr :: rest =>
    (fr.id = r.1 ∧ P fr r.2.1) ∨ (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest)

theorem Below.absent (D : FnId → BlockId → Local → Prop) (r : Rep) :
    ∀ (sk : List Frame) (above : Pointer × Option BlockId),
      (∀ (x : Frame), x ∈ sk → ¬ x.id = r.1) → Below D r above sk
  | [], _, _ => True.intro
  | fr :: rest, _, h =>
    Or.inr
      (And.intro (h fr (List.mem_cons.mpr (Or.inl rfl)))
        (Below.absent D r rest (fr.dest, fr.target)
          (fun x m => h x (List.mem_cons.mpr (Or.inr m)))))

/-- What the relation says of one entry, for the frames `sk` of the target's state. -/
structure Good (B : FnId → Local → Prop) (D : FnId → BlockId → Local → Prop)
    (g : List (Nat × FnId)) (P : Frame → Local → Prop) (sk : List Frame) (r : Rep) : Prop where
  slot : SlotOk (PtrOk B g) r.2.2
  unb : ∃ fn, fnOfIds g r.1 = some fn ∧ ¬ B fn r.2.1
  dests : ∀ (c : Frame), c ∈ sk → atPtr c.dest r = true ∨ Off r.1 r.2.1 c.dest
  placed : Placed D P r sk

/-- No pointer the invariant allows is into the entry's slot. -/
theorem Good.off {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {sk : List Frame} {r : Rep}
    (h : Good B D g P sk r) {n : Nat} (hk : KeysBelow g n) :
    ∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q := by
  have ⟨fn, hf, hb⟩ := h.unb
  have pos : 0 < r.1 := KeysBelow.pos hk r.1 fn hf
  have hpos : ¬ r.1 = 0 := by omega
  exact ptrOk_off B g r.1 r.2.1 fn hf hb hpos

/-- The entry's frame is one already made. -/
theorem Good.lt {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {sk : List Frame} {r : Rep}
    (h : Good B D g P sk r) {n : Nat} (hk : KeysBelow g n) : r.1 < n := by
  have ⟨fn, hf, _⟩ := h.unb
  have ⟨e, he, hkey⟩ := fnOfIds_key g r.1 fn hf
  have lt : e.1 < n := And.right (And.right hk e he)
  omega

/-- What is asked of the running frame's entries can be weakened. -/
theorem Good.imp {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P Q : Frame → Local → Prop} {fr : Frame} {rest : List Frame}
    {r : Rep} (h : Good B D g P (fr :: rest) r)
    (hpq : fr.id = r.1 → P fr r.2.1 → Q fr r.2.1) : Good B D g Q (fr :: rest) r := by
  have hp : (fr.id = r.1 ∧ P fr r.2.1) ∨
      (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := h.placed
  have hq : (fr.id = r.1 ∧ Q fr r.2.1) ∨
      (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := by
    cases hp with
    | inl a => exact Or.inl (And.intro (And.left a) (hpq (And.left a) (And.right a)))
    | inr b => exact Or.inr b
  exact { slot := h.slot, unb := h.unb, dests := h.dests, placed := hq }

/-- The running frame at another block. -/
theorem Good.atBlock {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P Q : Frame → Local → Prop} {fr : Frame} {rest : List Frame}
    {r : Rep} (x : BlockId) (h : Good B D g P (fr :: rest) r)
    (hpq : P fr r.2.1 → Q (fr.withBlock x) r.2.1) : Good B D g Q (fr.withBlock x :: rest) r := by
  have hp : (fr.id = r.1 ∧ P fr r.2.1) ∨
      (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := h.placed
  have hq : ((fr.withBlock x).id = r.1 ∧ Q (fr.withBlock x) r.2.1) ∨
      (¬ (fr.withBlock x).id = r.1 ∧
        Below D r ((fr.withBlock x).dest, (fr.withBlock x).target) rest) := by
    cases hp with
    | inl a => exact Or.inl (And.intro (And.left a) (hpq (And.right a)))
    | inr b => exact Or.inr b
  have hd : ∀ (c : Frame), c ∈ fr.withBlock x :: rest →
      atPtr c.dest r = true ∨ Off r.1 r.2.1 c.dest := by
    intro c mc
    cases List.mem_cons.mp mc with
    | inl e =>
      rw [e]
      exact h.dests fr (List.mem_cons.mpr (Or.inl rfl))
    | inr m' => exact h.dests c (List.mem_cons.mpr (Or.inr m'))
  exact { slot := h.slot, unb := h.unb, dests := hd, placed := hq }

/-- The relation between the source's state `s` and the target's `t`. `D fn b l`: the local
`l` is dead on entry to block `b` of `fn`. -/
def Rel (B : FnId → Local → Prop) (D : FnId → BlockId → Local → Prop) (s t : State) : Prop :=
  ∃ (reps : List Rep) (g : List (Nat × FnId)),
    s = puts t reps ∧ Borrows B g t ∧ Ordered t.skel ∧
      ∀ (r : Rep), r ∈ reps → Good B D g (fun fr l => D fr.fn fr.block l) t.skel r

/-! What is known of the target's state while a block runs. -/

structure Ctx (B : FnId → Local → Prop) (g : List (Nat × FnId)) (fr : Frame)
    (rest : List Frame) (t : State) : Prop where
  okAt : OkAt B g t
  keys : KeysBelow g t.next
  skel : t.skel = fr :: rest
  ordered : Ordered (fr :: rest)

theorem Ctx.next {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) {t' : State} (hs : OkAt B g t')
    (hn : t'.next = t.next) (hk : t'.skel = t.skel) : Ctx B g fr rest t' :=
  { okAt := hs, keys := KeysBelow.of_next hn h.keys, skel := hk.trans h.skel,
    ordered := h.ordered }

theorem Ctx.top {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) : t.top = fr.id :=
  State.top_of_skel t fr rest h.skel

theorem Ctx.topFn {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) : t.topFn = fr.fn :=
  State.topFn_of_skel t fr rest h.skel

theorem mem_skel_ids (st : State) (x : Frame) (m : x ∈ st.skel) : (x.id, x.fn) ∈ st.ids := by
  have m' : x ∈ st.stack.map bare := m
  have ⟨fr, hfr, e⟩ := List.mem_map.mp m'
  rw [← e]
  show ((bare fr).id, (bare fr).fn) ∈ st.stack.map idFnOf
  exact List.mem_map.mpr ⟨fr, hfr, rfl⟩

/-- Every frame's id is one already given. -/
theorem Ctx.lt {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) (x : Frame) (m : x ∈ fr :: rest) :
    x.id < t.next := by
  have m' : x ∈ t.skel := by
    rw [h.skel]
    exact m
  have hc : fnOfIds g x.id = some x.fn := And.left h.okAt (x.id, x.fn) (mem_skel_ids t x m')
  have ⟨e, he, hkey⟩ := fnOfIds_key g x.id x.fn hc
  have lt : e.1 < t.next := And.right (And.right h.keys e he)
  omega

theorem Ctx.cov {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) : fnOfIds g fr.id = some fr.fn := by
  have m' : fr ∈ t.skel := by
    rw [h.skel]
    exact List.mem_cons.mpr (Or.inl rfl)
  exact And.left h.okAt (fr.id, fr.fn) (mem_skel_ids t fr m')

theorem Ctx.pos {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) : ¬ fr.id = 0 := by
  have pos : 0 < fr.id := KeysBelow.pos h.keys fr.id fr.fn h.cov
  omega

/-- A place is in a frame already made. -/
theorem Ctx.place_lt {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (h : Ctx B g fr rest t) (p : Place) (q : Pointer)
    (hq : evalPlace t p = Outcome.ok q) : q.frame < t.next := by
  have hm : MemOk (PtrOk B g) t := And.right h.okAt
  cases evalProj_frame hm p.proj { frame := t.top, slot := p.base, path := [] } q hq with
  | inl same =>
    have same' : q.frame = t.top := same
    have lt : fr.id < t.next := h.lt fr (List.mem_cons.mpr (Or.inl rfl))
    have top : t.top = fr.id := h.top
    omega
  | inr ok => exact ptrOk_lt h.keys ok

/-- The entry can be carried through what the running frame evaluates, when its being dead
there says the thing does not mention its local. -/
theorem Good.can {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {fr : Frame} {rest : List Frame}
    {t : State} {r : Rep} (h : Good B D g P (fr :: rest) r) (ctx : Ctx B g fr rest t)
    (fine : Local → Bool) (hf : P fr r.2.1 → fine r.2.1 = true) : RepCan B g t.top fine r := by
  have hp : (fr.id = r.1 ∧ P fr r.2.1) ∨
      (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := h.placed
  have third : (fr.id = r.1 ∧ fine r.2.1 = true) ∨ ¬ r.1 = fr.id := by
    cases hp with
    | inl a => exact Or.inl (And.intro (And.left a) (hf (And.right a)))
    | inr b => exact Or.inr (fun e => And.left b e.symm)
  have third' : (t.top = r.1 ∧ fine r.2.1 = true) ∨ ¬ r.1 = t.top := by
    rw [ctx.top]
    exact third
  exact And.intro h.slot (And.intro (h.off ctx.keys) third')

/-! Jumps. -/

/-- A jump to a return target that gives a state had a target. -/
theorem jump?_ok (st t' : State) :
    ∀ (target : Option BlockId), st.jump? target = Outcome.ok (Next.state t') →
      ∃ tb, target = some tb ∧ st.jump tb = Outcome.ok (Next.state t')
  | none, h => by
    have bad : (Outcome.undefined Undefined.illFormed : Outcome Next) =
        Outcome.ok (Next.state t') := h
    cases bad
  | some tb, h => Exists.intro tb (And.intro rfl h)

theorem borrows_jump {B : FnId → Local → Prop} {g : List (Nat × FnId)} (x : BlockId) :
    ∀ (st : State), Borrows B g st → Borrows B g (st.mapStack (withTopBlock x))
  | { base := _, stack := [], next := _ }, h => h
  | { base := base, stack := fr :: rest, next := next }, h => by
    have hcov : Covers g { base := base, stack := fr :: rest, next := next } :=
      And.left (And.left h)
    have hmem : MemOk (PtrOk B g) { base := base, stack := fr :: rest, next := next } :=
      And.right (And.left h)
    have hcov' : Covers g { base := base, stack := fr.withBlock x :: rest, next := next } := hcov
    have hmem' : MemOk (PtrOk B g)
        { base := base, stack := fr.withBlock x :: rest, next := next } :=
      And.intro (And.left hmem) (fun fr' m =>
        (List.mem_cons.mp m).elim
          (fun e => by
            rw [e]
            exact And.right hmem fr (List.mem_cons.mpr (Or.inl rfl)))
          (fun m' => And.right hmem fr' (List.mem_cons.mpr (Or.inr m'))))
    exact And.intro (And.intro hcov' hmem') (And.right h)

/-- The states after a jump are related, when each of the running frame's entries is dead on
entry to the block. -/
theorem rel_jump {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {fr : Frame} {rest : List Frame} {t : State}
    (ctx : Ctx B g fr rest t) (x : BlockId) (reps : List Rep)
    (hg : ∀ (r : Rep), r ∈ reps → Good B D g (fun _ l => D fr.fn x l) (fr :: rest) r) :
    Rel B D (puts (t.mapStack (withTopBlock x)) reps) (t.mapStack (withTopBlock x)) := by
  have hsk : (t.mapStack (withTopBlock x)).skel = fr.withBlock x :: rest :=
    State.skel_jump x t fr rest ctx.skel
  have hb : Borrows B g (t.mapStack (withTopBlock x)) :=
    borrows_jump x t (And.intro ctx.okAt ctx.keys)
  have ho : Ordered (t.mapStack (withTopBlock x)).skel := by
    rw [hsk]
    exact ordered_withBlock fr rest x ctx.ordered
  have goods : ∀ (r : Rep), r ∈ reps →
      Good B D g (fun fr l => D fr.fn fr.block l) (t.mapStack (withTopBlock x)).skel r := by
    intro r m
    rw [hsk]
    exact Good.atBlock x (hg r m) (fun d => d)
  exact Exists.intro reps (Exists.intro g (And.intro rfl (And.intro hb (And.intro ho goods))))

theorem jump_rel {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {fr : Frame} {rest : List Frame} {t : State}
    (ctx : Ctx B g fr rest t) (x : BlockId) (reps : List Rep)
    (hg : ∀ (r : Rep), r ∈ reps → Good B D g (fun _ l => D fr.fn x l) (fr :: rest) r) :
    StepRel (Rel B D) ((puts t reps).jump x) (t.jump x) := by
  have e1 : (puts t reps).jump x =
      Outcome.ok (Next.state (puts (t.mapStack (withTopBlock x)) reps)) :=
    (jump_puts x reps t).trans
      (congrTo (Outcome.map (Next.map (fun s => puts s reps)))
        (State.jump_cons x t fr rest ctx.skel))
  have e2 : t.jump x = Outcome.ok (Next.state (t.mapStack (withTopBlock x))) :=
    State.jump_cons x t fr rest ctx.skel
  rw [e1, e2]
  exact StepRel.state (puts (t.mapStack (withTopBlock x)) reps) (t.mapStack (withTopBlock x))
    (rel_jump ctx x reps hg)

theorem switchOn_rel {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {fr : Frame} {rest : List Frame} {t : State}
    (ctx : Ctx B g fr rest t) (arms : List (Int × BlockId)) (otherwise : BlockId)
    (reps : List Rep)
    (hg : ∀ (key : Int) (r : Rep), r ∈ reps →
      Good B D g (fun _ l => D fr.fn (switchTarget key arms otherwise) l) (fr :: rest) r) :
    ∀ (key : Option Int),
      StepRel (Rel B D) (switchOn (puts t reps) arms otherwise key)
        (switchOn t arms otherwise key)
  | some key => jump_rel ctx (switchTarget key arms otherwise) reps (hg key)
  | none => StepRel.undefined Undefined.illTyped

theorem assertOn_rel {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {fr : Frame} {rest : List Frame} {t : State}
    (ctx : Ctx B g fr rest t) (expected : Bool) (kind : Error) (target : BlockId)
    (reps : List Rep)
    (hg : ∀ (r : Rep), r ∈ reps → Good B D g (fun _ l => D fr.fn target l) (fr :: rest) r) :
    ∀ (v : Value),
      StepRel (Rel B D) (assertOn (puts t reps) expected kind target v)
        (assertOn t expected kind target v)
  | Value.bool b => by
    by_cases h : b = expected
    · have left : assertOn (puts t reps) expected kind target (Value.bool b) =
          (puts t reps).jump target :=
        (assertOn_bool (puts t reps) expected kind target b).trans (if_pos h)
      have right : assertOn t expected kind target (Value.bool b) = t.jump target :=
        (assertOn_bool t expected kind target b).trans (if_pos h)
      rw [left, right]
      exact jump_rel ctx target reps hg
    · have left : assertOn (puts t reps) expected kind target (Value.bool b) =
          Outcome.panic kind :=
        (assertOn_bool (puts t reps) expected kind target b).trans (if_neg h)
      have right : assertOn t expected kind target (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool t expected kind target b).trans (if_neg h)
      rw [left, right]
      exact StepRel.panic kind
  | Value.unit => StepRel.undefined Undefined.illTyped
  | Value.int _ _ => StepRel.undefined Undefined.illTyped
  | Value.adt _ _ => StepRel.undefined Undefined.illTyped
  | Value.array _ => StepRel.undefined Undefined.illTyped
  | Value.ref _ => StepRel.undefined Undefined.illTyped
  | Value.dead => StepRel.undefined Undefined.illTyped

/-! Returns. -/

theorem borrows_pop {B : FnId → Local → Prop} {g : List (Nat × FnId)} :
    ∀ (st : State), Borrows B g st → Borrows B g st.pop
  | { base := _, stack := [], next := _ }, h => h
  | { base := base, stack := fr :: rest, next := next }, h => by
    have hmem : MemOk (PtrOk B g) { base := base, stack := fr :: rest, next := next } :=
      And.right (And.left h)
    have below : Borrows B g { base := base, stack := rest, next := next } :=
      And.intro
        (And.intro (fun e he => And.left (And.left h) e (List.mem_cons.mpr (Or.inr he)))
          (And.intro (And.left hmem)
            (fun fr' m => And.right hmem fr' (List.mem_cons.mpr (Or.inr m)))))
        (And.right h)
    exact below

/-- After the returned value is read: the states the return gives are related. -/
theorem ret_after {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {fr : Frame} (reps : List Rep)
    (v : Value) (hv : Value.Ok (PtrOk B g) v) (t' : State) :
    ∀ (rest : List Frame) (t : State), Ctx B g fr rest t →
      (∀ (r : Rep), r ∈ reps → Good B D g P (fr :: rest) r) →
        retVia v t.pop (fr :: rest) = Outcome.ok (Next.state t') →
          Rel B D (puts t' (dropAt fr.dest reps)) t'
  | [], _, _, _, h => by
    have h' : (Outcome.ok (Next.done v) : Outcome Next) = Outcome.ok (Next.state t') := h
    cases h'
  | caller :: rest', t, ctx, hg, h => by
    have h' : (t.pop.write fr.dest v).bind (fun st1 => st1.jump? fr.target) =
        Outcome.ok (Next.state t') := h
    have ⟨st1, hw, hj⟩ := Outcome.bind_eq_ok _ _ _ h'
    have hj' : st1.jump? fr.target = Outcome.ok (Next.state t') := hj
    have hpop : Borrows B g t.pop := borrows_pop t (And.intro ctx.okAt ctx.keys)
    have hb1 : Borrows B g st1 := State.write_borrows hpop fr.dest v hv st1 hw
    have ho : fr.dest.frame < fr.id ∧
        (∀ (x : Frame), x ∈ caller :: rest' → x.id < fr.id) ∧ Ordered (caller :: rest') :=
      ctx.ordered
    have ctx1 : Ctx B g caller rest' st1 :=
      { okAt := And.left hb1, keys := And.right hb1,
        skel := (State.write_skel t.pop fr.dest v st1 hw).trans
          (State.pop_skel t fr (caller :: rest') ctx.skel),
        ordered := And.right (And.right ho) }
    have ⟨tb, htg, hj2⟩ := jump?_ok st1 t' fr.target hj'
    have e : st1.mapStack (withTopBlock tb) = t' :=
      Next.state.inj
        (Outcome.ok.inj ((State.jump_cons tb st1 caller rest' ctx1.skel).symm.trans hj2))
    have goods : ∀ (r : Rep), r ∈ dropAt fr.dest reps →
        Good B D g (fun _ l => D caller.fn tb l) (caller :: rest') r := by
      intro r m
      have mem := mem_dropAt fr.dest reps r m
      have G := hg r (And.left mem)
      have notAt : ¬ atPtr fr.dest r = true := And.right mem
      have hp : (fr.id = r.1 ∧ P fr r.2.1) ∨
          (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) (caller :: rest')) := G.placed
      have placed : (caller.id = r.1 ∧ D caller.fn tb r.2.1) ∨
          (¬ caller.id = r.1 ∧ Below D r (caller.dest, caller.target) rest') := by
        cases hp with
        | inl gone =>
          have same : fr.id = r.1 := And.left gone
          have lt : caller.id < fr.id :=
            And.left (And.right ho) caller (List.mem_cons.mpr (Or.inl rfl))
          have ne : ¬ caller.id = r.1 := by omega
          have absent : ∀ (x : Frame), x ∈ rest' → ¬ x.id = r.1 := by
            intro x mx
            have ltx : x.id < fr.id :=
              And.left (And.right ho) x (List.mem_cons.mpr (Or.inr mx))
            omega
          exact Or.inr
            (And.intro ne (Below.absent D r rest' (caller.dest, caller.target) absent))
        | inr under =>
          have hb : (caller.id = r.1 ∧
                (atPtr fr.dest r = true ∨
                  OptLive (fun tb => D caller.fn tb r.2.1) fr.target)) ∨
              (¬ caller.id = r.1 ∧ Below D r (caller.dest, caller.target) rest') :=
            And.right under
          cases hb with
          | inl here =>
            cases And.right here with
            | inl isAt => exact absurd isAt notAt
            | inr live =>
              rw [htg] at live
              exact Or.inl (And.intro (And.left here) live)
          | inr deeper => exact Or.inr deeper
      exact { slot := G.slot, unb := G.unb,
              dests := fun c mc => G.dests c (List.mem_cons.mpr (Or.inr mc)),
              placed := placed }
    rw [← e]
    exact rel_jump ctx1 tb (dropAt fr.dest reps) goods

/-- A return keeps the relation, when no entry of the running frame is its local 0. -/
theorem ret_rel {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {fr : Frame} {rest : List Frame}
    {t : State} (ctx : Ctx B g fr rest t) (reps : List Rep)
    (hg : ∀ (r : Rep), r ∈ reps → Good B D g P (fr :: rest) r)
    (h0 : ∀ (r : Rep), r ∈ reps → fr.id = r.1 → ¬ r.2.1 = 0) :
    StepRel (Rel B D) (puts t reps).ret t.ret := by
  have ho : fr.dest.frame < fr.id ∧ (∀ (x : Frame), x ∈ rest → x.id < fr.id) ∧ Ordered rest :=
    ctx.ordered
  have hu : ∀ (x : Frame), x ∈ rest → ¬ x.id = fr.id := by
    intro x m
    have lt : x.id < fr.id := And.left (And.right ho) x m
    omega
  have off0 : ∀ (r : Rep), r ∈ reps →
      Off r.1 r.2.1 { frame := fr.id, slot := 0, path := [] } := by
    intro r m
    by_cases e : fr.id = r.1
    · exact Or.inr (h0 r m e)
    · exact Or.inl (fun e' => e e'.symm)
  have hd : ∀ (r : Rep), r ∈ reps → atPtr fr.dest r = true ∨ Off r.1 r.2.1 fr.dest :=
    fun r m => (hg r m).dests fr (List.mem_cons.mpr (Or.inl rfl))
  rw [ret_puts t fr rest ctx.skel ctx.pos hu reps off0 hd]
  refine StepRel.of_map (Rel B D) (fun x => puts x (dropAt fr.dest reps)) t.ret ?_
  intro t' ht'
  have et : t.ret =
      (t.slot { frame := fr.id, slot := 0, path := [] }).bind (fun slot =>
        slot.load.bind (fun v => retVia v t.pop (fr :: rest))) :=
    State.ret_skel t fr rest ctx.skel ctx.pos
  have h1 : (t.slot { frame := fr.id, slot := 0, path := [] }).bind (fun slot =>
      slot.load.bind (fun v => retVia v t.pop (fr :: rest))) = Outcome.ok (Next.state t') :=
    et.symm.trans ht'
  have ⟨slot, hslot, h2⟩ := Outcome.bind_eq_ok _ _ _ h1
  have h2' : slot.load.bind (fun v => retVia v t.pop (fr :: rest)) =
      Outcome.ok (Next.state t') := h2
  have ⟨v, hv, h3⟩ := Outcome.bind_eq_ok _ _ _ h2'
  have h3' : retVia v t.pop (fr :: rest) = Outcome.ok (Next.state t') := h3
  have sok : SlotOk (PtrOk B g) slot :=
    State.slot_ok (And.right ctx.okAt) { frame := fr.id, slot := 0, path := [] } slot hslot
  have vok : Value.Ok (PtrOk B g) v := Slot.load_ok (PtrOk B g) slot v sok hv
  exact ret_after reps v vok t' rest t ctx hg h3'

/-! Calls. -/

/-- A call of a function, when it gives a state, enters it. -/
theorem callOn_fn_all (sig : FnId → Option FnSig) (st : State) (id : FnId) (args : List Value)
    (dest : Pointer) (target : Option BlockId) :
    Outcome.All (fun (n : Next) => ∃ s, n = Next.state (enterState st id s args dest target))
      (callOn sig st (Callee.fn id) args dest target) := by
  simp only [callOn]
  cases hs : sig id with
  | none => exact Outcome.all_undefined _ _
  | some s =>
    by_cases hl : args.length = s.args
    · rw [State.enter_eq, if_pos hl]
      exact Outcome.all_ok (Exists.intro s rfl)
    · rw [State.enter_eq, if_neg hl]
      exact Outcome.all_undefined _ _

/-- The state a call of a function enters is related. -/
theorem enter_rel {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {fr : Frame} {rest : List Frame}
    {t : State} (ctx : Ctx B g fr rest t) (reps : List Rep)
    (hg : ∀ (r : Rep), r ∈ reps → Good B D g P (fr :: rest) r) (id : FnId) (s : FnSig)
    (args : List Value) (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk B g) v)
    (dest : Pointer) (hdlt : dest.frame < t.next) (target : Option BlockId)
    (hd : ∀ (r : Rep), r ∈ reps → atPtr dest r = true ∨ Off r.1 r.2.1 dest)
    (hlive : ∀ (r : Rep), r ∈ reps → fr.id = r.1 →
      atPtr dest r = true ∨ OptLive (fun tb => D fr.fn tb r.2.1) target) :
    Rel B D (puts (enterState t id s args dest target) reps)
      (enterState t id s args dest target) := by
  have hb : Borrows B ((t.next, id) :: g) (enterState t id s args dest target) :=
    enterState_borrows (And.intro ctx.okAt ctx.keys) id s args dest target hargs
  have hsk : (enterState t id s args dest target).skel =
      bare (newFrame t id s args dest target) :: fr :: rest :=
    congrTo (fun k => bare (newFrame t id s args dest target) :: k) ctx.skel
  have ord : Ordered (bare (newFrame t id s args dest target) :: fr :: rest) :=
    And.intro hdlt (And.intro (fun x m => ctx.lt x m) ctx.ordered)
  have ho : Ordered (enterState t id s args dest target).skel := by
    rw [hsk]
    exact ord
  have grow : ∀ (p : Pointer), PtrOk B g p → PtrOk B ((t.next, id) :: g) p :=
    fun p hp => ptrOk_grow B g t.next ctx.keys id p hp
  have goods : ∀ (r : Rep), r ∈ reps →
      Good B D ((t.next, id) :: g) (fun fr l => D fr.fn fr.block l)
        (enterState t id s args dest target).skel r := by
    intro r m
    have G := hg r m
    have ⟨fn, hf, hbn⟩ := G.unb
    have lt : r.1 < t.next := G.lt ctx.keys
    have ne : ¬ t.next = r.1 := by omega
    have hp : (fr.id = r.1 ∧ P fr r.2.1) ∨
        (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := G.placed
    have below : (fr.id = r.1 ∧
          (atPtr dest r = true ∨ OptLive (fun tb => D fr.fn tb r.2.1) target)) ∨
        (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := by
      cases hp with
      | inl here => exact Or.inl (And.intro (And.left here) (hlive r m (And.left here)))
      | inr under => exact Or.inr under
    have hdests : ∀ (c : Frame), c ∈ bare (newFrame t id s args dest target) :: fr :: rest →
        atPtr c.dest r = true ∨ Off r.1 r.2.1 c.dest := by
      intro c mc
      cases List.mem_cons.mp mc with
      | inl e =>
        rw [e]
        exact hd r m
      | inr mc' => exact G.dests c mc'
    rw [hsk]
    exact { slot := slotOk_mono grow r.2.2 G.slot,
            unb := Exists.intro fn (And.intro (fnOfIds_grow g t.next ctx.keys id r.1 fn hf) hbn),
            dests := hdests,
            placed := Or.inr (And.intro ne below) }
  exact Exists.intro reps
    (Exists.intro ((t.next, id) :: g) (And.intro rfl (And.intro hb (And.intro ho goods))))

/-- A call keeps the relation, from the state after its operands with the destination
evaluated. -/
theorem callOn_rel {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {P : Frame → Local → Prop} {fr : Frame} {rest : List Frame}
    {t : State} (sig : FnId → Option FnSig) (ctx : Ctx B g fr rest t) (reps : List Rep)
    (hg : ∀ (r : Rep), r ∈ reps → Good B D g P (fr :: rest) r)
    (args : List Value) (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk B g) v)
    (dest : Pointer) (hdlt : dest.frame < t.next)
    (hd : ∀ (r : Rep), r ∈ reps → atPtr dest r = true ∨ Off r.1 r.2.1 dest)
    (callee : Callee) (target : Option BlockId)
    (hlive : ∀ (r : Rep), r ∈ reps → fr.id = r.1 →
      atPtr dest r = true ∨ OptLive (fun tb => D fr.fn tb r.2.1) target) :
    StepRel (Rel B D) (callOn sig (puts t reps) callee args dest target)
      (callOn sig t callee args dest target) := by
  have hnext : ∀ (r : Rep), r ∈ reps → ¬ r.1 = t.next := by
    intro r m
    have lt : r.1 < t.next := (hg r m).lt ctx.keys
    omega
  have hrefs : ∀ (p : Pointer), Value.ref p ∈ args → ∀ (r : Rep), r ∈ reps → Off r.1 r.2.1 p :=
    fun p mp r m =>
      (hg r m).off ctx.keys p
        (asRef_ok (PtrOk B g) (Value.ref p) p (hargs (Value.ref p) mp) rfl)
  rw [callOn_puts sig reps t args dest target hnext hrefs hd callee]
  refine StepRel.of_map (Rel B D) (fun x => puts x (callReps callee dest reps))
    (callOn sig t callee args dest target) ?_
  intro t' ht'
  cases callee with
  | fn id =>
    have ⟨s, e⟩ := callOn_fn_all sig t id args dest target (Next.state t') ht'
    have e' : t' = enterState t id s args dest target := Next.state.inj e
    rw [e']
    exact enter_rel ctx reps hg id s args hargs dest hdlt target hd hlive
  | intrinsic i =>
    have h' : (i.eval t args).bind (fun v =>
        (t.write dest v).bind (fun st1 => st1.jump? target)) = Outcome.ok (Next.state t') := ht'
    have ⟨v, hv, h2⟩ := Outcome.bind_eq_ok _ _ _ h'
    have h2' : (t.write dest v).bind (fun st1 => st1.jump? target) =
        Outcome.ok (Next.state t') := h2
    have ⟨st1, hw, hj⟩ := Outcome.bind_eq_ok _ _ _ h2'
    have hj' : st1.jump? target = Outcome.ok (Next.state t') := hj
    have vok : Value.Ok (PtrOk B g) v := intrinsicEval_ok (PtrOk B g) t i args hargs v hv
    have hb1 : Borrows B g st1 :=
      State.write_borrows (And.intro ctx.okAt ctx.keys) dest v vok st1 hw
    have ctx1 : Ctx B g fr rest st1 :=
      ctx.next (And.left hb1) (State.write_next t dest v st1 hw)
        (State.write_skel t dest v st1 hw)
    have ⟨tb, htg, hj2⟩ := jump?_ok st1 t' target hj'
    have e : st1.mapStack (withTopBlock tb) = t' :=
      Next.state.inj
        (Outcome.ok.inj ((State.jump_cons tb st1 fr rest ctx1.skel).symm.trans hj2))
    have goods : ∀ (r : Rep), r ∈ dropAt dest reps →
        Good B D g (fun _ l => D fr.fn tb l) (fr :: rest) r := by
      intro r m
      have mem := mem_dropAt dest reps r m
      exact (hg r (And.left mem)).imp (fun same _ => by
        have live : atPtr dest r = true ∨ OptLive (fun tb => D fr.fn tb r.2.1) target :=
          hlive r (And.left mem) same
        rw [htg] at live
        cases live with
        | inl isAt => exact absurd isAt (And.right mem)
        | inr d => exact d)
    rw [← e]
    exact rel_jump ctx1 tb (dropAt dest reps) goods

/-! The certificate: what it says of a local dead at a point. -/

def pointsOr : Option (List (List (List Local))) → List (List (List Local))
  | some pts => pts
  | none => []

/-- The certificate's points of one function. -/
def ptsOf (cert : List (List (List (List Local)))) (fn : FnId) : List (List (List Local)) :=
  pointsOr (nth? cert fn)

def firstPoint : List (List Local) → List Local
  | [] => []
  | p :: _ => p

theorem liveIn_first (points : List (List (List Local))) (b : BlockId)
    (bpts : List (List Local)) (h : nth? points b = some bpts) :
    liveIn points b = firstPoint bpts := by
  unfold liveIn
  rw [h]
  cases bpts with
  | nil => rfl
  | cons p rest => rfl

/-- The local is one the function has, not live on entry to the block. -/
def DeadIn (prog : Program) (cert : List (List (List (List Local)))) (fn : FnId) (b : BlockId)
    (l : Local) : Prop :=
  ∃ body, nth? prog.fns fn = some body ∧ l < body.locals.length ∧
    has l (liveIn (ptsOf cert fn) b) = false

/-- The local is one of the function's `n`, not live at the point. -/
def Here (n : Nat) (cur : List Local) (l : Local) : Prop := l < n ∧ has l cur = false

/-- What the check says of a local dead before a terminator. -/
theorem termLive_dead (n : Nat) (points : List (List (List Local))) (last : List Local)
    (term : Terminator) (l : Local) (h : termLive n points last term = true)
    (hl : Here n last l) :
    termFine l term = true ∧
      (termWrites l term = true ∨
        (successors term).all (fun t => !has l (liveIn points t)) = true) := by
  have hl' : l < n ∧ has l last = false := hl
  have ok : (fun l =>
      has l last ||
        (termFine l term &&
          (termWrites l term || (successors term).all (fun t => !has l (liveIn points t))))) l =
        true :=
    allBelowP_lt _ l n h (And.left hl')
  have ok' : (has l last ||
      (termFine l term &&
        (termWrites l term || (successors term).all (fun t => !has l (liveIn points t))))) =
        true := ok
  rw [And.right hl'] at ok'
  have rest : (termFine l term &&
      (termWrites l term || (successors term).all (fun t => !has l (liveIn points t)))) =
        true := ok'
  exact And.intro (and_true_left rest) (or_true_cases (and_true_right rest))

/-- A local dead before a terminator that does not write it is dead on entry to each block
the terminator goes to. -/
theorem here_succ (prog : Program) (cert : List (List (List (List Local)))) (fn : FnId)
    (body : Body) (hbody : nth? prog.fns fn = some body) (last : List Local) (term : Terminator)
    (hterm : termLive body.locals.length (ptsOf cert fn) last term = true) (l : Local)
    (hl : Here body.locals.length last l) (hw : termWrites l term = false) (x : BlockId)
    (hx : x ∈ successors term) : DeadIn prog cert fn x l := by
  have hl' : l < body.locals.length ∧ has l last = false := hl
  cases And.right (termLive_dead body.locals.length (ptsOf cert fn) last term l hterm hl) with
  | inl w => exact false_eq_true_elim (hw.symm.trans w)
  | inr all =>
    have nb : (!has l (liveIn (ptsOf cert fn) x)) = true :=
      all_mem (fun t => !has l (liveIn (ptsOf cert fn) t)) (successors term) all x hx
    exact Exists.intro body (And.intro hbody (And.intro (And.left hl') (not_true_false nb)))

/-! The terminator. -/

/-- A call, after its operands: the destination, then `callOn_rel`. -/
theorem call_rel (prog : Program) (cert : List (List (List (List Local))))
    (sig : FnId → Option FnSig) {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (body : Body) (hbody : nth? prog.fns fr.fn = some body)
    (last : List Local) (reps : List Rep) (callee : Callee) (args : List Operand) (dest : Place)
    (target : Option BlockId)
    (hterm : termLive body.locals.length (ptsOf cert fr.fn) last
      (Terminator.call callee args dest target) = true)
    (ctx : Ctx B g fr rest t)
    (hg : ∀ (r : Rep), r ∈ reps →
      Good B (DeadIn prog cert) g (fun _ l => Here body.locals.length last l) (fr :: rest) r)
    (vals : List Value) (hvals : ∀ (v : Value), v ∈ vals → Value.Ok (PtrOk B g) v) :
    StepRel (Rel B (DeadIn prog cert))
      ((evalPlace (puts t reps) dest).bind (fun ptr =>
        callOn sig (puts t reps) callee vals ptr target))
      ((evalPlace t dest).bind (fun ptr => callOn sig t callee vals ptr target)) := by
  have rd : ∀ (r : Rep), r ∈ reps → RepDest B g t.top dest r := by
    intro r m
    have G := hg r m
    have hp : (fr.id = r.1 ∧ Here body.locals.length last r.2.1) ∨
        (¬ fr.id = r.1 ∧ Below (DeadIn prog cert) r (fr.dest, fr.target) rest) := G.placed
    have third : (fr.id = r.1 ∧ (placeFine r.2.1 dest = true ∨ dest = Place.mk r.2.1 [])) ∨
        ¬ r.1 = fr.id := by
      cases hp with
      | inl a =>
        have fine : termFine r.2.1 (Terminator.call callee args dest target) = true :=
          And.left
            (termLive_dead body.locals.length (ptsOf cert fr.fn) last
              (Terminator.call callee args dest target) r.2.1 hterm (And.right a))
        have fd : (placeFine r.2.1 dest || wholeDest r.2.1 dest) = true := and_true_right fine
        cases or_true_cases fd with
        | inl pf => exact Or.inl (And.intro (And.left a) (Or.inl pf))
        | inr wd => exact Or.inl (And.intro (And.left a) (Or.inr (wholeDest_eq r.2.1 dest wd)))
      | inr b => exact Or.inr (fun e => And.left b e.symm)
    have third' : (t.top = r.1 ∧ (placeFine r.2.1 dest = true ∨ dest = Place.mk r.2.1 [])) ∨
        ¬ r.1 = t.top := by
      rw [ctx.top]
      exact third
    exact And.intro G.slot (And.intro (G.off ctx.keys) third')
  have pl : evalPlace (puts t reps) dest = evalPlace t dest ∧
      ∀ (q : Pointer), evalPlace t dest = Outcome.ok q → ∀ (r : Rep), r ∈ reps →
        q = { frame := r.1, slot := r.2.1, path := [] } ∨ Off r.1 r.2.1 q :=
    evalDest_puts reps t ctx.okAt dest rd
  exact StepRel.bind_same (Rel B (DeadIn prog cert)) (And.left pl) (fun ptr hptr => by
    have hd : ∀ (r : Rep), r ∈ reps → atPtr ptr r = true ∨ Off r.1 r.2.1 ptr := by
      intro r m
      cases And.right pl ptr hptr r m with
      | inl e => exact Or.inl (decide_eq_true e)
      | inr o => exact Or.inr o
    have hlive : ∀ (r : Rep), r ∈ reps → fr.id = r.1 →
        atPtr ptr r = true ∨
          OptLive (fun tb => DeadIn prog cert fr.fn tb r.2.1) target := by
      intro r m same
      have hp : (fr.id = r.1 ∧ Here body.locals.length last r.2.1) ∨
          (¬ fr.id = r.1 ∧ Below (DeadIn prog cert) r (fr.dest, fr.target) rest) :=
        (hg r m).placed
      cases hp with
      | inr b => exact absurd same (And.left b)
      | inl a =>
        have hl : Here body.locals.length last r.2.1 := And.right a
        have hl' : r.2.1 < body.locals.length ∧ has r.2.1 last = false := hl
        cases And.right
            (termLive_dead body.locals.length (ptsOf cert fr.fn) last
              (Terminator.call callee args dest target) r.2.1 hterm hl) with
        | inl w =>
          have w' : wholeDest r.2.1 dest = true := w
          have ed : dest = Place.mk r.2.1 [] := wholeDest_eq r.2.1 dest w'
          have ep : evalPlace t (Place.mk r.2.1 []) = Outcome.ok ptr := by
            rw [← ed]
            exact hptr
          have e1 : Pointer.mk t.top r.2.1 [] = ptr := Outcome.ok.inj ep
          have e2 : Pointer.mk t.top r.2.1 [] = Pointer.mk r.1 r.2.1 [] :=
            congrTo (fun k => Pointer.mk k r.2.1 []) (ctx.top.trans same)
          have e3 : ptr = { frame := r.1, slot := r.2.1, path := [] } := e1.symm.trans e2
          exact Or.inl (decide_eq_true e3)
        | inr all =>
          have all' : (optList target).all
              (fun t => !has r.2.1 (liveIn (ptsOf cert fr.fn) t)) = true := all
          exact Or.inr
            (optLive_of (fun tb => DeadIn prog cert fr.fn tb r.2.1) target (fun x hx =>
              Exists.intro body
                (And.intro hbody
                  (And.intro (And.left hl')
                    (not_true_false
                      (all_mem (fun t => !has r.2.1 (liveIn (ptsOf cert fr.fn) t))
                        (optList target) all' x hx))))))
    exact callOn_rel sig ctx reps hg vals hvals ptr (ctx.place_lt dest ptr hptr) hd callee
      target hlive)

/-- The terminator keeps the relation, from states whose running-frame entries are dead
before it. -/
theorem execTerm_rel (prog : Program) (cert : List (List (List (List Local))))
    (sig : FnId → Option FnSig) {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (body : Body) (hbody : nth? prog.fns fr.fn = some body)
    (last : List Local) (reps : List Rep) (ctx : Ctx B g fr rest t)
    (hg : ∀ (r : Rep), r ∈ reps →
      Good B (DeadIn prog cert) g (fun _ l => Here body.locals.length last l) (fr :: rest) r) :
    ∀ (term : Terminator),
      termLive body.locals.length (ptsOf cert fr.fn) last term = true →
        StepRel (Rel B (DeadIn prog cert)) (execTerm sig (puts t reps) term)
          (execTerm sig t term)
  | Terminator.goto x, hterm =>
    jump_rel ctx x reps (fun r m =>
      (hg r m).imp (fun _ hl =>
        here_succ prog cert fr.fn body hbody last (Terminator.goto x) hterm r.2.1 hl rfl x
          (List.mem_cons.mpr (Or.inl rfl))))
  | Terminator.switchInt d arms otherwise, hterm =>
    StepRel.bind_map (Rel B (DeadIn prog cert))
      (evalOperand_puts d reps t ctx.okAt (fun r m =>
        (hg r m).can ctx (fun l => operandFine l d) (fun hl =>
          And.left
            (termLive_dead body.locals.length (ptsOf cert fr.fn) last
              (Terminator.switchInt d arms otherwise) r.2.1 hterm hl))))
      (fun a ha =>
        switchOn_rel
          (ctx.next (And.right (evalOperand_okAt t ctx.okAt d a ha)) (evalOperand_next t d a ha)
            (evalOperand_skel t d a ha))
          arms otherwise reps
          (fun key r m =>
            (hg r m).imp (fun _ hl =>
              here_succ prog cert fr.fn body hbody last (Terminator.switchInt d arms otherwise)
                hterm r.2.1 hl rfl (switchTarget key arms otherwise)
                (switchTarget_mem key otherwise arms)))
          (switchKey a.1))
  | Terminator.call callee args dest target, hterm =>
    StepRel.bind_map (Rel B (DeadIn prog cert))
      (evalOperands_puts args reps t ctx.okAt (fun r m =>
        (hg r m).can ctx (fun l => operandsFine l args) (fun hl =>
          and_true_left
            (And.left
              (termLive_dead body.locals.length (ptsOf cert fr.fn) last
                (Terminator.call callee args dest target) r.2.1 hterm hl)))))
      (fun a ha =>
        call_rel prog cert sig body hbody last reps callee args dest target hterm
          (ctx.next (And.right (evalOperands_okAt args t ctx.okAt a ha))
            (evalOperands_next args t a ha) (evalOperands_skel args t a ha))
          hg a.1 (And.left (evalOperands_okAt args t ctx.okAt a ha)))
  | Terminator.ret, hterm =>
    ret_rel ctx reps hg (fun r m same => by
      have hp : (fr.id = r.1 ∧ Here body.locals.length last r.2.1) ∨
          (¬ fr.id = r.1 ∧ Below (DeadIn prog cert) r (fr.dest, fr.target) rest) :=
        (hg r m).placed
      cases hp with
      | inr b => exact absurd same (And.left b)
      | inl a =>
        have fine : termFine r.2.1 Terminator.ret = true :=
          And.left
            (termLive_dead body.locals.length (ptsOf cert fr.fn) last Terminator.ret r.2.1 hterm
              (And.right a))
        have ne : ¬ r.2.1 = 0 := of_decide_eq_true fine
        exact ne)
  | Terminator.assert cond expected kind target, hterm =>
    StepRel.bind_map (Rel B (DeadIn prog cert))
      (evalOperand_puts cond reps t ctx.okAt (fun r m =>
        (hg r m).can ctx (fun l => operandFine l cond) (fun hl =>
          And.left
            (termLive_dead body.locals.length (ptsOf cert fr.fn) last
              (Terminator.assert cond expected kind target) r.2.1 hterm hl))))
      (fun a ha =>
        assertOn_rel
          (ctx.next (And.right (evalOperand_okAt t ctx.okAt cond a ha))
            (evalOperand_next t cond a ha) (evalOperand_skel t cond a ha))
          expected kind target reps
          (fun r m =>
            (hg r m).imp (fun _ hl =>
              here_succ prog cert fr.fn body hbody last
                (Terminator.assert cond expected kind target) hterm r.2.1 hl rfl target
                (List.mem_cons.mpr (Or.inl rfl))))
          a.1)
  | Terminator.drop p x, hterm =>
    jump_rel ctx x reps (fun r m =>
      (hg r m).imp (fun _ hl =>
        here_succ prog cert fr.fn body hbody last (Terminator.drop p x) hterm r.2.1 hl rfl x
          (List.mem_cons.mpr (Or.inl rfl))))
  | Terminator.unreachable, _ => StepRel.undefined Undefined.unreachable

/-! The block. -/

/-- The entry can be carried through or absorbed by a statement the certificate checked. -/
theorem fits_of {B : FnId → Local → Prop} {D : FnId → BlockId → Local → Prop}
    {g : List (Nat × FnId)} {fr : Frame} {rest : List Frame} {t : State}
    (ctx : Ctx B g fr rest t) (n : Nat) (before after : List Local) (s : Statement)
    (hs : stmtLive n before after s = true) (r : Rep)
    (G : Good B D g (fun _ l => Here n before l) (fr :: rest) r) : RepFits B g fr.id s r := by
  have hp : (fr.id = r.1 ∧ Here n before r.2.1) ∨
      (¬ fr.id = r.1 ∧ Below D r (fr.dest, fr.target) rest) := G.placed
  have third : absorbs fr.id s r = true ∨
      (fr.id = r.1 ∧ stmtFine r.2.1 s = true) ∨ ¬ r.1 = fr.id := by
    cases hp with
    | inl a =>
      have hh : r.2.1 < n ∧ has r.2.1 before = false := And.right a
      cases stmtLive_dead n before after s r.2.1 hs (And.left hh) (And.right hh) with
      | inl over => exact Or.inl (and_true_intro (decide_eq_true (And.left a)) over)
      | inr stays => exact Or.inr (Or.inl (And.intro (And.left a) (And.left stays)))
    | inr b => exact Or.inr (Or.inr (fun e => And.left b e.symm))
  exact And.intro G.slot (And.intro (G.off ctx.keys) third)

/-- What the source does at a store the pass removes: undefined behaviour, or the state
with one more entry, the stored local's, dead after the store. -/
theorem dead_store_src {B : FnId → Local → Prop} {g : List (Nat × FnId)} {fr : Frame}
    {rest : List Frame} {t : State} (adts : List AdtDecl) (ctx : Ctx B g fr rest t) (n : Nat)
    (bw : Local → Bool) (after : List Local) (reps : List Rep)
    (hslots : ∀ (r : Rep), r ∈ reps → SlotOk (PtrOk B g) r.2.2) :
    ∀ (s : Statement), StmtFine B fr.fn s → deadStore n bw after s = true →
      (∃ why, execStmt adts (puts t reps) s = Outcome.undefined why) ∨
        ∃ (l : Local) (v : Value),
          execStmt adts (puts t reps) s =
              Outcome.ok (puts t (reps ++ [(fr.id, l, Slot.init v)])) ∧
            Value.Ok (PtrOk B g) v ∧ l < n ∧ bw l = false ∧ has l after = false ∧
              ∀ (l' : Local), isOverwrite l' s = true → l' = l
  | Statement.assign { base := base, proj := proj } rv, hB, hk => by
    have hk' : (decide (proj = []) &&
        (decide (base < n) && (!bw base && (rvalueCalm rv && !has base after)))) = true := hk
    have hp : proj = [] := of_decide_eq_true (and_true_left hk')
    have hlt : base < n := of_decide_eq_true (and_true_left (and_true_right hk'))
    have hnb : bw base = false :=
      not_true_false (and_true_left (and_true_right (and_true_right hk')))
    have hcalm : rvalueCalm rv = true :=
      and_true_left (and_true_right (and_true_right (and_true_right hk')))
    have hdl : has base after = false :=
      not_true_false (and_true_right (and_true_right (and_true_right (and_true_right hk'))))
    subst hp
    have hS : OkAt B g (puts t reps) := puts_okAt reps t ctx.okAt hslots
    have hfnS : (puts t reps).topFn = fr.fn := (puts_topFn reps t).trans ctx.topFn
    have htopS : (puts t reps).top = fr.id := (puts_top reps t).trans ctx.top
    have block : ∀ (o : Outcome (Value × State)), evalRvalue adts (puts t reps) rv = o →
        execStmt adts (puts t reps) (Statement.assign (Place.mk base []) rv) =
          o.bind (fun r => r.2.write { frame := r.2.top, slot := base, path := [] } r.1) :=
      fun o e =>
        congrTo
          (fun x => Outcome.bind x (fun (r : Value × State) =>
            r.2.write { frame := r.2.top, slot := base, path := [] } r.1))
          e
    have over : ∀ (l' : Local),
        isOverwrite l' (Statement.assign (Place.mk base []) rv) = true → l' = base := by
      intro l' ho
      have eb : base = l' := of_decide_eq_true (and_true_left ho)
      exact eb.symm
    cases evalRvalue_calm adts (puts t reps) rv hcalm with
    | inr hu =>
      have ⟨why, e⟩ := hu
      exact Or.inl ⟨why, block (Outcome.undefined why) e⟩
    | inl hv =>
      have ⟨v, e⟩ := hv
      have vok : Value.Ok (PtrOk B g) v :=
        And.left (evalRvalue_okAt adts (puts t reps) hS fr.fn hfnS rv hB (v, puts t reps) e)
      have stmt : execStmt adts (puts t reps) (Statement.assign (Place.mk base []) rv) =
          (puts t reps).write { frame := fr.id, slot := base, path := [] } v :=
        (block (Outcome.ok (v, puts t reps)) e).trans
          (congrTo (fun k => (puts t reps).write { frame := k, slot := base, path := [] } v)
            htopS)
      cases State.write_whole (puts t reps) fr.id base v with
      | inl written =>
        have e2 : (puts t reps).put fr.id base (Slot.init v) =
            puts t (reps ++ [(fr.id, base, Slot.init v)]) :=
          (puts_append reps t (fr.id, base, Slot.init v)).symm
        have done : execStmt adts (puts t reps) (Statement.assign (Place.mk base []) rv) =
            Outcome.ok (puts t (reps ++ [(fr.id, base, Slot.init v)])) :=
          stmt.trans (written.trans (congrTo Outcome.ok e2))
        exact Or.inr ⟨base, v, done, vok, hlt, hnb, hdl, over⟩
      | inr hu =>
        have ⟨why, e2⟩ := hu
        exact Or.inl ⟨why, stmt.trans e2⟩
  | Statement.storageLive _, _, hk => false_eq_true_elim hk
  | Statement.storageDead _, _, hk => false_eq_true_elim hk
  | Statement.nop, _, hk => false_eq_true_elim hk

/-- The block: from related states whose running-frame entries are dead at the block's first
point, the source's block is undefined or the two blocks' outcomes are related. -/
theorem block_rel (prog : Program) (cert : List (List (List (List Local))))
    (borrowed : Body → Local → Bool) {g : List (Nat × FnId)} {fr : Frame} {rest : List Frame}
    (body : Body) (hbody : nth? prog.fns fr.fn = some body) (term : Terminator) :
    ∀ (stmts : List Statement) (pts : List (List Local)) (t : State) (reps : List Rep),
      stmtsLive body.locals.length pts stmts = true →
      termLive body.locals.length (ptsOf cert fr.fn) (lastPoint pts) term = true →
      Ctx (BorrowedIn borrowed prog) g fr rest t →
      (∀ (s : Statement), s ∈ stmts → StmtFine (BorrowedIn borrowed prog) fr.fn s) →
      (∀ (r : Rep), r ∈ reps →
        Good (BorrowedIn borrowed prog) (DeadIn prog cert) g
          (fun _ l => Here body.locals.length (firstPoint pts) l) (fr :: rest) r) →
        (∃ why, execBlockFrom prog.adts prog.sig (puts t reps) stmts term =
            Outcome.undefined why) ∨
          StepRel (Rel (BorrowedIn borrowed prog) (DeadIn prog cert))
            (execBlockFrom prog.adts prog.sig (puts t reps) stmts term)
            (execBlockFrom prog.adts prog.sig t
              (killDead body.locals.length (borrowed body) pts stmts) term)
  | [], [], _, _, hl, _, _, _, _ => false_eq_true_elim hl
  | [], [last], t, reps, _, hterm, ctx, _, hg =>
    Or.inr (execTerm_rel prog cert prog.sig body hbody last reps ctx hg term hterm)
  | [], _ :: _ :: _, _, _, hl, _, _, _, _ => false_eq_true_elim hl
  | _ :: _, [], _, _, hl, _, _, _, _ => false_eq_true_elim hl
  | _ :: _, [_], _, _, hl, _, _, _, _ => false_eq_true_elim hl
  | s :: ss, before :: after :: more, t, reps, hl, hterm, ctx, hB, hg => by
    have hl' : (stmtLive body.locals.length before after s &&
        stmtsLive body.locals.length (after :: more) ss) = true := hl
    have hs : stmtLive body.locals.length before after s = true := and_true_left hl'
    have hrest : stmtsLive body.locals.length (after :: more) ss = true := and_true_right hl'
    have hterm' : termLive body.locals.length (ptsOf cert fr.fn) (lastPoint (after :: more))
        term = true := hterm
    have hsB : StmtFine (BorrowedIn borrowed prog) fr.fn s :=
      hB s (List.mem_cons.mpr (Or.inl rfl))
    have hBrest : ∀ (s' : Statement), s' ∈ ss → StmtFine (BorrowedIn borrowed prog) fr.fn s' :=
      fun s' m => hB s' (List.mem_cons.mpr (Or.inr m))
    have hg' : ∀ (r : Rep), r ∈ reps →
        Good (BorrowedIn borrowed prog) (DeadIn prog cert) g
          (fun _ l => Here body.locals.length before l) (fr :: rest) r := hg
    have kd : killDead body.locals.length (borrowed body) (before :: after :: more) (s :: ss) =
        (if deadStore body.locals.length (borrowed body) after s = true then Statement.nop
          else s) ::
          killDead body.locals.length (borrowed body) (after :: more) ss := rfl
    rw [kd]
    by_cases hdead : deadStore body.locals.length (borrowed body) after s = true
    · rw [if_pos hdead]
      have src := dead_store_src prog.adts ctx body.locals.length (borrowed body) after reps
        (fun r m => (hg' r m).slot) s hsB hdead
      cases src with
      | inl hu =>
        have ⟨why, e⟩ := hu
        exact Or.inl ⟨why,
          congrTo
            (fun x => Outcome.bind x (fun st1 => execBlockFrom prog.adts prog.sig st1 ss term))
            e⟩
      | inr hok =>
        have ⟨l, v, e, vok, hlt, hnb, hdl, hover⟩ := hok
        have srcBlock : execBlockFrom prog.adts prog.sig (puts t reps) (s :: ss) term =
            execBlockFrom prog.adts prog.sig (puts t (reps ++ [(fr.id, l, Slot.init v)])) ss
              term :=
          congrTo
            (fun x => Outcome.bind x (fun st1 => execBlockFrom prog.adts prog.sig st1 ss term))
            e
        have tgtBlock : execBlockFrom prog.adts prog.sig t
              (Statement.nop ::
                killDead body.locals.length (borrowed body) (after :: more) ss) term =
            execBlockFrom prog.adts prog.sig t
              (killDead body.locals.length (borrowed body) (after :: more) ss) term := rfl
        have unb : ¬ BorrowedIn borrowed prog fr.fn l := by
          intro hbl
          have hb1 : borrowedOf borrowed (nth? prog.fns fr.fn) l = true := hbl
          rw [hbody] at hb1
          have hb2 : borrowed body l = true := hb1
          exact true_eq_false_elim (hb2.symm.trans hnb)
        have goods : ∀ (r : Rep), r ∈ reps ++ [(fr.id, l, Slot.init v)] →
            Good (BorrowedIn borrowed prog) (DeadIn prog cert) g
              (fun _ l => Here body.locals.length (firstPoint (after :: more)) l)
              (fr :: rest) r := by
          intro r m
          cases List.mem_append.mp m with
          | inl old =>
            exact (hg' r old).imp (fun _ hh => by
              have hh' : r.2.1 < body.locals.length ∧ has r.2.1 before = false := hh
              cases stmtLive_dead body.locals.length before after s r.2.1 hs (And.left hh')
                  (And.right hh') with
              | inl over =>
                have el : r.2.1 = l := hover r.2.1 over
                have dead : r.2.1 < body.locals.length ∧ has r.2.1 after = false := by
                  rw [el]
                  exact And.intro hlt hdl
                exact dead
              | inr stays =>
                have dead : r.2.1 < body.locals.length ∧ has r.2.1 after = false :=
                  And.intro (And.left hh') (And.right stays)
                exact dead)
          | inr new =>
            have er : r = (fr.id, l, Slot.init v) :=
              (List.mem_cons.mp new).elim (fun e => e) (fun m' => mem_nil_elim m')
            have hdests : ∀ (c : Frame), c ∈ fr :: rest →
                atPtr c.dest (fr.id, l, Slot.init v) = true ∨ Off fr.id l c.dest := by
              intro c mc
              have lt : c.dest.frame < fr.id := Ordered.dest_lt fr rest ctx.ordered c mc
              have ne : ¬ fr.id = c.dest.frame := by omega
              exact Or.inr (Or.inl ne)
            have dead : l < body.locals.length ∧ has l after = false := And.intro hlt hdl
            have fresh : Good (BorrowedIn borrowed prog) (DeadIn prog cert) g
                (fun _ l => Here body.locals.length (firstPoint (after :: more)) l)
                (fr :: rest) (fr.id, l, Slot.init v) :=
              { slot := vok,
                unb := Exists.intro fr.fn (And.intro ctx.cov unb),
                dests := hdests,
                placed := Or.inl (And.intro rfl dead) }
            rw [er]
            exact fresh
        rw [srcBlock, tgtBlock]
        exact block_rel prog cert borrowed body hbody term ss (after :: more) t
          (reps ++ [(fr.id, l, Slot.init v)]) hrest hterm' ctx hBrest goods
    · rw [if_neg hdead]
      have fits : ∀ (r : Rep), r ∈ reps → RepFits (BorrowedIn borrowed prog) g fr.id s r :=
        fun r m => fits_of ctx body.locals.length before after s hs r (hg' r m)
      have ex : execStmt prog.adts (puts t reps) s =
          Outcome.map (fun x => puts x (keepReps fr.id s reps)) (execStmt prog.adts t s) :=
        execStmt_absorb prog.adts fr.fn s hsB fr.id reps t ctx.okAt ctx.topFn ctx.top fits
      exact StepRel.bind_map_or (Rel (BorrowedIn borrowed prog) (DeadIn prog cert)) ex
        (fun t1 h1 =>
          block_rel prog cert borrowed body hbody term ss (after :: more) t1
            (keepReps fr.id s reps) hrest hterm'
            (ctx.next (execStmt_okAt prog.adts t ctx.okAt fr.fn ctx.topFn s hsB t1 h1)
              (execStmt_next prog.adts t s t1 h1) (execStmt_skel prog.adts t s t1 h1))
            hBrest
            (fun r m => by
              have mem := mem_keepReps fr.id s reps r m
              exact (hg' r (And.left mem)).imp (fun same hh => by
                have hh' : r.2.1 < body.locals.length ∧ has r.2.1 before = false := hh
                cases stmtLive_dead body.locals.length before after s r.2.1 hs (And.left hh')
                    (And.right hh') with
                | inl over =>
                  exact absurd (and_true_intro (decide_eq_true same) over) (And.right mem)
                | inr stays =>
                  have dead : r.2.1 < body.locals.length ∧ has r.2.1 after = false :=
                    And.intro (And.left hh') (And.right stays)
                  exact dead)))

end Blaze.Mir.Opt
