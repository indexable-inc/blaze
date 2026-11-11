import Blaze.Mir.Opt.BorrowExec
import Blaze.Mir.Opt.NextId
import Blaze.Mir.Opt.Rename

/-!
Every step keeps what a pointer may name: the invariant dead-store elimination rests on,
fourth piece.

`Borrows B g st`: `g` lists every frame the run has made, each with its function, those that
returned too; every id in it is below the id the next call's frame gets; it covers the
state's frames; and every pointer in memory names frame 0 or a local that the function of
its frame may borrow (`OkAt`). A call adds its frame to `g` and a return leaves `g`, so a
pointer that outlives its frame still names a local its function may borrow, and no pointer
names a frame not yet made.

`step_borrows`: when every `ref` of the program is of a place whose base its function may
borrow or that goes through a dereference (`ProgFine`), a step from a state the invariant
holds of gives one it holds of, for some longer `g`.
-/

namespace Blaze.Mir

theorem Outcome.All.and {α : Type} {P Q : α → Prop} {x : Outcome α} (hp : Outcome.All P x)
    (hq : Outcome.All Q x) : Outcome.All (fun a => P a ∧ Q a) x :=
  fun a h => And.intro (hp a h) (hq a h)

/-! `Value.Ok` in a weaker predicate. -/

theorem Value.Ok.mono {P Q : Pointer → Prop} (h : ∀ (p : Pointer), P p → Q p) :
    ∀ (v : Value), Value.Ok P v → Value.Ok Q v := by
  intro v hv
  induction hv with
  | unit => exact Value.Ok.unit
  | bool b => exact Value.Ok.bool b
  | int ty x => exact Value.Ok.int ty x
  | adt variant fields _ ih => exact Value.Ok.adt variant fields ih
  | array elems _ ih => exact Value.Ok.array elems ih
  | ref p hp => exact Value.Ok.ref p (h p hp)
  | dead => exact Value.Ok.dead

theorem slotOk_mono {P Q : Pointer → Prop} (h : ∀ (p : Pointer), P p → Q p) :
    ∀ (s : Slot), SlotOk P s → SlotOk Q s
  | Slot.init v, hs => Value.Ok.mono h v hs
  | Slot.uninit, _ => True.intro
  | Slot.moved, _ => True.intro

theorem slotsOk_mono {P Q : Pointer → Prop} (h : ∀ (p : Pointer), P p → Q p)
    {slots : List Slot} (hs : SlotsOk P slots) : SlotsOk Q slots :=
  fun s m => slotOk_mono h s (hs s m)

/-! The list of frames grows by the frame a call makes. -/

/-- Every id the list has is above 0 (frame 0 is no call's frame) and below `n`, and `n` is
above 0. -/
def KeysBelow (g : List (Nat × FnId)) (n : Nat) : Prop :=
  0 < n ∧ ∀ (e : Nat × FnId), e ∈ g → 0 < e.1 ∧ e.1 < n

theorem KeysBelow.of_next {g : List (Nat × FnId)} {n n' : Nat} (e : n' = n)
    (h : KeysBelow g n) : KeysBelow g n' := by
  rw [e]
  exact h

theorem fnOfIds_key :
    ∀ (g : List (Nat × FnId)) (id : Nat) (fn : FnId), fnOfIds g id = some fn →
      ∃ e, e ∈ g ∧ e.1 = id
  | [], _, _, h => none_eq_some_elim h
  | e :: rest, id, fn, h => by
    by_cases hc : e.1 = id
    · exact ⟨e, List.mem_cons.mpr (Or.inl rfl), hc⟩
    · have h' : fnOfIds rest id = some fn :=
        ((fnOfIds_cons e rest id).trans (if_neg hc)).symm.trans h
      have ⟨x, hx, hk⟩ := fnOfIds_key rest id fn h'
      exact ⟨x, List.mem_cons.mpr (Or.inr hx), hk⟩

/-- No frame in the list has id 0. -/
theorem KeysBelow.pos {g : List (Nat × FnId)} {n : Nat} (hk : KeysBelow g n) (id : Nat)
    (fn : FnId) : fnOfIds g id = some fn → 0 < id := by
  intro h
  have ⟨e, he, hkey⟩ := fnOfIds_key g id fn h
  rw [← hkey]
  exact And.left (And.right hk e he)

/-- A frame the list has is found as before when a new id is put in front. -/
theorem fnOfIds_grow (g : List (Nat × FnId)) (n : Nat) (hk : KeysBelow g n) (fn : FnId)
    (id : Nat) (f : FnId) (h : fnOfIds g id = some f) : fnOfIds ((n, fn) :: g) id = some f := by
  have ⟨e, he, hkey⟩ := fnOfIds_key g id f h
  have lt : e.1 < n := And.right (And.right hk e he)
  have ne : ¬ n = id := by omega
  exact ((fnOfIds_cons (n, fn) g id).trans (if_neg ne)).trans h

theorem ptrOk_grow (B : FnId → Local → Prop) (g : List (Nat × FnId)) (n : Nat)
    (hk : KeysBelow g n) (fn : FnId) (p : Pointer) (h : PtrOk B g p) :
    PtrOk B ((n, fn) :: g) p := by
  have h' : p.frame = 0 ∨ FrameHas B p.slot (fnOfIds g p.frame) := h
  cases h' with
  | inl zero => exact Or.inl zero
  | inr has =>
    cases hl : fnOfIds g p.frame with
    | none =>
      rw [hl] at has
      exact False.elim has
    | some f =>
      have grown : fnOfIds ((n, fn) :: g) p.frame = some f := fnOfIds_grow g n hk fn p.frame f hl
      have has' : FrameHas B p.slot (fnOfIds ((n, fn) :: g) p.frame) := by
        rw [grown]
        rw [hl] at has
        exact has
      exact Or.inr has'

/-! The invariant. -/

/-- `g` lists the frames the run has made, with ids below the next one, and memory holds
only pointers `PtrOk B g` allows. -/
def Borrows (B : FnId → Local → Prop) (g : List (Nat × FnId)) (st : State) : Prop :=
  OkAt B g st ∧ KeysBelow g st.next

/-- What a step gives: a state the invariant holds of, for some list. -/
def NextBorrows (B : FnId → Local → Prop) : Next → Prop
  | Next.state st => ∃ g, Borrows B g st
  | Next.done _ => True

theorem State.jump_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} (t : BlockId) :
    ∀ (st : State), Borrows B g st → Outcome.All (NextBorrows B) (st.jump t)
  | { base := _, stack := [], next := _ }, _ => Outcome.all_undefined _ _
  | { base := base, stack := fr :: rest, next := next }, h => by
    have hcov : Covers g { base := base, stack := fr :: rest, next := next } :=
      And.left (And.left h)
    have hmem : MemOk (PtrOk B g) { base := base, stack := fr :: rest, next := next } :=
      And.right (And.left h)
    have hcov' : Covers g { base := base, stack := fr.withBlock t :: rest, next := next } := hcov
    have hmem' : MemOk (PtrOk B g)
        { base := base, stack := fr.withBlock t :: rest, next := next } :=
      And.intro (And.left hmem) (fun fr' m =>
        (List.mem_cons.mp m).elim
          (fun e => by
            rw [e]
            exact And.right hmem fr (List.mem_cons.mpr (Or.inl rfl)))
          (fun m' => And.right hmem fr' (List.mem_cons.mpr (Or.inr m'))))
    exact Outcome.all_ok (Exists.intro g (And.intro (And.intro hcov' hmem') (And.right h)))

theorem State.jump?_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} (st : State)
    (h : Borrows B g st) :
    ∀ (target : Option BlockId), Outcome.All (NextBorrows B) (st.jump? target)
  | some t => State.jump_borrows t st h
  | none => Outcome.all_undefined _ _

theorem switchOn_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} (st : State)
    (h : Borrows B g st) (arms : List (Int × BlockId)) (otherwise : BlockId) :
    ∀ (key : Option Int), Outcome.All (NextBorrows B) (switchOn st arms otherwise key)
  | some key => State.jump_borrows (switchTarget key arms otherwise) st h
  | none => Outcome.all_undefined _ _

theorem assertOn_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} (st : State)
    (h : Borrows B g st) (expected : Bool) (kind : Error) (target : BlockId) :
    ∀ (v : Value), Outcome.All (NextBorrows B) (assertOn st expected kind target v)
  | Value.bool b => by
    by_cases hb : b = expected
    · rw [assertOn_bool, if_pos hb]
      exact State.jump_borrows target st h
    · rw [assertOn_bool, if_neg hb]
      exact Outcome.all_panic _ _
  | Value.unit => Outcome.all_undefined _ _
  | Value.int _ _ => Outcome.all_undefined _ _
  | Value.adt _ _ => Outcome.all_undefined _ _
  | Value.array _ => Outcome.all_undefined _ _
  | Value.ref _ => Outcome.all_undefined _ _
  | Value.dead => Outcome.all_undefined _ _

/-- A write of a value the invariant allows keeps the invariant. -/
theorem State.write_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (h : Borrows B g st) (p : Pointer) (v : Value) (hv : Value.Ok (PtrOk B g) v) :
    Outcome.All (Borrows B g) (st.write p v) :=
  fun st' hw =>
    And.intro
      (And.intro (Covers.of_ids (State.write_ids st p v st' hw) (And.left (And.left h)))
        (State.write_ok (And.right (And.left h)) p v hv st' hw))
      (KeysBelow.of_next (State.write_next st p v st' hw) (And.right h))

/-- The state a call enters holds the invariant, with the new frame in the list. -/
theorem enterState_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (h : Borrows B g st) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk B g) v) :
    Borrows B ((st.next, id) :: g) (enterState st id sig args dest target) := by
  have hcov : Covers g st := And.left (And.left h)
  have hmem : MemOk (PtrOk B g) st := And.right (And.left h)
  have hk : KeysBelow g st.next := And.right h
  have grow : ∀ (p : Pointer), PtrOk B g p → PtrOk B ((st.next, id) :: g) p :=
    fun p hp => ptrOk_grow B g st.next hk id p hp
  have cov : Covers ((st.next, id) :: g) (enterState st id sig args dest target) := by
    intro e he
    have he' : e ∈ (st.next, id) :: st.ids := he
    cases List.mem_cons.mp he' with
    | inl here =>
      rw [here]
      exact (fnOfIds_cons (st.next, id) g st.next).trans (if_pos rfl)
    | inr there => exact fnOfIds_grow g st.next hk id e.1 e.2 (hcov e there)
  have locals : SlotsOk (PtrOk B ((st.next, id) :: g))
      (Slot.uninit ::
        (args.map Slot.init ++ List.replicate (sig.locals - 1 - sig.args) Slot.uninit)) := by
    intro s hs
    cases List.mem_cons.mp hs with
    | inl here =>
      rw [here]
      exact True.intro
    | inr there =>
      cases List.mem_append.mp there with
      | inl inArgs =>
        have ⟨v, hv, e⟩ := List.mem_map.mp inArgs
        rw [← e]
        exact Value.Ok.mono grow v (hargs v hv)
      | inr inRest =>
        rw [mem_replicate' Slot.uninit s (sig.locals - 1 - sig.args) inRest]
        exact True.intro
  have mem : MemOk (PtrOk B ((st.next, id) :: g)) (enterState st id sig args dest target) :=
    And.intro (slotsOk_mono grow (And.left hmem)) (fun fr m =>
      (List.mem_cons.mp m).elim
        (fun e => by
          rw [e]
          exact locals)
        (fun m' => slotsOk_mono grow (And.right hmem fr m')))
  have keys : KeysBelow ((st.next, id) :: g) (st.next + 1) := by
    refine And.intro (Nat.succ_pos st.next) ?_
    intro e he
    cases List.mem_cons.mp he with
    | inl here =>
      rw [here]
      exact And.intro (And.left hk) (Nat.lt_succ_self st.next)
    | inr there =>
      exact And.intro (And.left (And.right hk e there))
        (Nat.lt_succ_of_lt (And.right (And.right hk e there)))
  exact And.intro (And.intro cov mem) keys

theorem State.enter_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (h : Borrows B g st) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk B g) v) :
    Outcome.All (NextBorrows B) (st.enter id sig args dest target) := by
  by_cases hl : args.length = sig.args
  · rw [State.enter_eq, if_pos hl]
    exact Outcome.all_ok
      (Exists.intro ((st.next, id) :: g) (enterState_borrows h id sig args dest target hargs))
  · rw [State.enter_eq, if_neg hl]
    exact Outcome.all_undefined _ _

/-- A return: the frame goes, the list stays, and what is returned is a value the invariant
allows. -/
theorem State.ret_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)} (st : State)
    (h : Borrows B g st) : Outcome.All (NextBorrows B) st.ret := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => exact Outcome.all_undefined _ _
    | cons fr rest =>
      cases fr with
      | mk fid ffn fblock locals dest target =>
        cases locals with
        | nil => exact Outcome.all_undefined _ _
        | cons slot locals =>
          have hmem : MemOk (PtrOk B g)
              { base := base,
                stack :=
                  { id := fid, fn := ffn, block := fblock, locals := slot :: locals,
                    dest := dest, target := target } :: rest,
                next := next } := And.right (And.left h)
          have hslot : SlotOk (PtrOk B g) slot :=
            And.right hmem
              { id := fid, fn := ffn, block := fblock, locals := slot :: locals, dest := dest,
                target := target }
              (List.mem_cons.mpr (Or.inl rfl)) slot (List.mem_cons.mpr (Or.inl rfl))
          cases rest with
          | nil =>
            exact Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok True.intro)
          | cons caller rest =>
            have below : Borrows B g { base := base, stack := caller :: rest, next := next } :=
              And.intro
                (And.intro
                  (fun e he => And.left (And.left h) e (List.mem_cons.mpr (Or.inr he)))
                  (And.intro (And.left hmem)
                    (fun fr' m => And.right hmem fr' (List.mem_cons.mpr (Or.inr m)))))
                (And.right h)
            exact Outcome.all_bind (fun v hv => Slot.load_ok (PtrOk B g) slot v hslot hv)
              (fun v hv =>
                Outcome.all_bind (State.write_borrows below dest v hv) (fun st1 h1 =>
                  State.jump?_borrows st1 h1 target))

theorem saturating_ok (P : Pointer → Prop) (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All (Value.Ok P) (saturating op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact Outcome.all_undefined _ _

theorem saturatingOp_ok (P : Pointer → Prop) (op : BinOp) (a b : Value) :
    Outcome.All (Value.Ok P) (saturatingOp op a b) := by
  cases a <;> cases b <;>
    first
    | exact withSame_ok P _ _ _ (fun x y => saturating_ok P _ x y)
    | exact Outcome.all_undefined _ _

/-- An intrinsic gives a value with no pointer in it, or the value its arguments make. -/
theorem intrinsicEval_ok (P : Pointer → Prop) (st : State) (i : Intrinsic) (args : List Value)
    (h : ∀ (v : Value), v ∈ args → Value.Ok P v) :
    Outcome.All (Value.Ok P) (i.eval st args) := by
  unfold Intrinsic.eval
  split <;>
    first
    | exact Outcome.all_panic _ _
    | exact Outcome.all_bind (Outcome.all_true _) (fun v _ => lenOf_ok P v)
    | exact binaryOp_ok P _ _ _ _
    | exact saturatingOp_ok P _ _ _
    | exact Outcome.all_ok (Value.Ok.adt _ _ (by assumption))
    | exact Outcome.all_undefined _ _

theorem callOn_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)}
    (sig : FnId → Option FnSig) (st : State) (h : Borrows B g st) (args : List Value)
    (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk B g) v) (dest : Pointer)
    (target : Option BlockId) :
    ∀ (callee : Callee),
      Outcome.All (NextBorrows B) (callOn sig st callee args dest target)
  | Callee.fn id => by
    simp only [callOn]
    cases hs : sig id with
    | none => exact Outcome.all_undefined _ _
    | some s => exact State.enter_borrows h id s args dest target hargs
  | Callee.intrinsic i =>
    Outcome.all_bind (intrinsicEval_ok (PtrOk B g) st i args hargs) (fun v hv =>
      Outcome.all_bind (State.write_borrows h dest v hv) (fun st1 h1 =>
        State.jump?_borrows st1 h1 target))

theorem execTerm_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)}
    (sig : FnId → Option FnSig) (st : State) (h : Borrows B g st) :
    ∀ (term : Terminator), Outcome.All (NextBorrows B) (execTerm sig st term)
  | Terminator.goto t => State.jump_borrows t st h
  | Terminator.switchInt discr arms otherwise =>
    Outcome.all_bind
      (Outcome.All.and (evalOperand_okAt st (And.left h) discr) (evalOperand_next st discr))
      (fun r hr =>
        switchOn_borrows r.2
          (And.intro (And.right (And.left hr)) (KeysBelow.of_next (And.right hr) (And.right h)))
          arms otherwise (switchKey r.1))
  | Terminator.call callee args dest target =>
    Outcome.all_bind
      (Outcome.All.and (evalOperands_okAt args st (And.left h)) (evalOperands_next args st))
      (fun r hr =>
        Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
          callOn_borrows sig r.2
            (And.intro (And.right (And.left hr)) (KeysBelow.of_next (And.right hr) (And.right h)))
            r.1 (And.left (And.left hr)) ptr target callee))
  | Terminator.ret => State.ret_borrows st h
  | Terminator.assert cond expected kind target =>
    Outcome.all_bind
      (Outcome.All.and (evalOperand_okAt st (And.left h) cond) (evalOperand_next st cond))
      (fun r hr =>
        assertOn_borrows r.2
          (And.intro (And.right (And.left hr)) (KeysBelow.of_next (And.right hr) (And.right h)))
          expected kind target r.1)
  | Terminator.drop _ t => State.jump_borrows t st h
  | Terminator.unreachable => Outcome.all_undefined _ _

theorem execBlockFrom_borrows {B : FnId → Local → Prop} {g : List (Nat × FnId)}
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (fn : FnId) (term : Terminator) :
    ∀ (stmts : List Statement) (st : State), Borrows B g st → st.topFn = fn →
      (∀ (s : Statement), s ∈ stmts → StmtFine B fn s) →
        Outcome.All (NextBorrows B) (execBlockFrom adts sig st stmts term)
  | [], st, h, _, _ => execTerm_borrows sig st h term
  | s :: rest, st, h, hfn, hf =>
    Outcome.all_bind
      (Outcome.All.and
        (execStmt_okAt adts st (And.left h) fn hfn s (hf s (List.mem_cons.mpr (Or.inl rfl))))
        (Outcome.All.and (execStmt_next adts st s) (execStmt_ctl adts st s)))
      (fun st1 h1 =>
        execBlockFrom_borrows adts sig fn term rest st1
          (And.intro (And.left h1)
            (KeysBelow.of_next (And.left (And.right h1)) (And.right h)))
          ((State.topFn_of_ctl (And.right (And.right h1))).trans hfn)
          (fun s' m => hf s' (List.mem_cons.mpr (Or.inr m))))

/-- Every `ref` of the program is of a place whose base its function may borrow, or of a
place through a dereference. -/
def ProgFine (B : FnId → Local → Prop) (prog : Program) : Prop :=
  ∀ (fn : FnId) (b : BlockId) (blk : BasicBlock), prog.block fn b = some blk →
    ∀ (s : Statement), s ∈ blk.stmts → StmtFine B fn s

/-- A step keeps the invariant, for a list that has the frame a call made. -/
theorem step_borrows {B : FnId → Local → Prop} {prog : Program} (hf : ProgFine B prog)
    {g : List (Nat × FnId)} :
    ∀ (st : State), Borrows B g st → Outcome.All (NextBorrows B) (step prog st)
  | { base := _, stack := [], next := _ }, _ => Outcome.all_undefined _ _
  | { base := base, stack := fr :: rest, next := next }, h => by
    show Outcome.All (NextBorrows B)
      (execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
        (prog.block fr.fn fr.block))
    cases hb : prog.block fr.fn fr.block with
    | none => exact Outcome.all_undefined _ _
    | some blk =>
      exact execBlockFrom_borrows prog.adts prog.sig fr.fn blk.terminator blk.stmts
        { base := base, stack := fr :: rest, next := next } h rfl (hf fr.fn fr.block blk hb)

end Blaze.Mir
