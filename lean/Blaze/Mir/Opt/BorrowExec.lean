import Blaze.Mir.Opt.Borrow

/-!
Statements keep what a pointer may name: the invariant dead-store elimination rests on,
third piece.

`OkAt B ids st`: the state's frames are `ids` and every pointer in its memory is one
`PtrOk B ids` allows. An operator gives a value with no pointer in it; an operand, a value
read from memory; a `ref`, the pointer its place evaluates to, which `PtrOk` allows when the
place's base local may be borrowed or the place goes through a dereference (`RvalueFine`:
the Rust pass's "ever borrowed", as a condition on the body). So a statement of a body that
meets the condition keeps `OkAt` (`execStmt_okAt`).

Calls and returns are the next piece.
-/

namespace Blaze.Mir

/-! Operators give values with no pointer in them. -/

theorem intResult_ok (P : Pointer → Prop) {ty : ScalarTy} (r : Result (Scalar ty)) :
    Outcome.All (Value.Ok P) (intResult r) :=
  Outcome.all_bind (Outcome.all_true _) (fun v _ => Outcome.all_ok (Value.Ok.int ty v))

theorem sameBinary_ok (P : Pointer → Prop) (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All (Value.Ok P) (sameBinary op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact Outcome.all_ok (Value.Ok.bool _)
    | exact intResult_ok P _

theorem sameChecked_ok (P : Pointer → Prop) (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All (Value.Ok P) (sameChecked op a b) := by
  cases op <;>
    first
    | exact intResult_ok P _
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact Outcome.all_ok (Value.Ok.bool _)

theorem sameOr_ok (P : Pointer → Prop) {ty : ScalarTy} (g : Scalar ty → Outcome Value)
    (hg : ∀ (y : Scalar ty), Outcome.All (Value.Ok P) (g y)) :
    ∀ (o : Option (Scalar ty)), Outcome.All (Value.Ok P) (sameOr o g)
  | some b => hg b
  | none => Outcome.all_undefined _ _

theorem withSame_ok (P : Pointer → Prop) {ty ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty')
    (f : Scalar ty → Scalar ty → Outcome Value)
    (hf : ∀ (x y : Scalar ty), Outcome.All (Value.Ok P) (f x y)) :
    Outcome.All (Value.Ok P) (withSame a b f) :=
  sameOr_ok P (f a) (hf a) (asTy ty b)

theorem intBinary_ok (P : Pointer → Prop) (checked : Bool) (op : BinOp) {ty ty' : ScalarTy}
    (a : Scalar ty) (b : Scalar ty') : Outcome.All (Value.Ok P) (intBinary checked op a b) := by
  cases checked <;> cases op <;>
    first
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact intResult_ok P _
    | exact withSame_ok P a b _ (fun x y => sameBinary_ok P _ x y)
    | exact withSame_ok P a b _ (fun x y => sameChecked_ok P _ x y)

theorem boolBinary_ok (P : Pointer → Prop) (op : BinOp) (a b : Bool) :
    Outcome.All (Value.Ok P) (boolBinary op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (Value.Ok.bool _)
    | exact Outcome.all_undefined _ _

theorem binaryOp_ok (P : Pointer → Prop) (checked : Bool) (op : BinOp) (a b : Value) :
    Outcome.All (Value.Ok P) (binaryOp checked op a b) := by
  cases a <;> cases b <;>
    first
    | exact intBinary_ok P _ _ _ _
    | exact boolBinary_ok P _ _ _
    | exact Outcome.all_undefined _ _

theorem ok_pair (P : Pointer → Prop) {x y : Value} (hx : Value.Ok P x) (hy : Value.Ok P y) :
    Value.Ok P (Value.adt 0 [x, y]) :=
  Value.Ok.adt 0 [x, y] (fun v hv =>
    (List.mem_cons.mp hv).elim (fun e => Value.Ok.of_eq e hx)
      (fun m =>
        (List.mem_cons.mp m).elim (fun e => Value.Ok.of_eq e hy) (fun m2 => mem_nil_elim m2)))

theorem sameOverflowing_ok (P : Pointer → Prop) (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All (Value.Ok P) (sameOverflowing op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (ok_pair P (Value.Ok.int _ _) (Value.Ok.bool _))
    | exact Outcome.all_undefined _ _

theorem overflowingOp_ok (P : Pointer → Prop) (op : BinOp) (a b : Value) :
    Outcome.All (Value.Ok P) (overflowingOp op a b) := by
  cases a <;> cases b <;>
    first
    | exact withSame_ok P _ _ _ (fun x y => sameOverflowing_ok P _ x y)
    | exact Outcome.all_undefined _ _

theorem unaryOp_ok (P : Pointer → Prop) (op : UnOp) (v : Value) :
    Outcome.All (Value.Ok P) (unaryOp op v) := by
  cases v <;> cases op <;>
    first
    | exact Outcome.all_ok (Value.Ok.bool _)
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact intResult_ok P _
    | exact Outcome.all_undefined _ _

theorem castTo_ok (P : Pointer → Prop) (target : ScalarTy) (v : Value) :
    Outcome.All (Value.Ok P) (castTo target v) := by
  cases v <;>
    first
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact Outcome.all_undefined _ _

theorem lenOf_ok (P : Pointer → Prop) (v : Value) : Outcome.All (Value.Ok P) (lenOf v) := by
  cases v <;>
    first
    | exact Outcome.all_ok (Value.Ok.int _ _)
    | exact Outcome.all_undefined _ _

theorem discriminantOf_ok (P : Pointer → Prop) (adts : List AdtDecl) (adt : Nat) (v : Value) :
    Outcome.All (Value.Ok P) (discriminantOf adts adt v) := by
  cases v with
  | adt variant fields =>
    cases hd : nth? adts adt with
    | none =>
      simp only [discriminantOf, hd]
      exact Outcome.all_undefined _ _
    | some decl =>
      cases hv : nth? decl.variants variant with
      | none =>
        simp only [discriminantOf, hd, hv]
        exact Outcome.all_undefined _ _
      | some entry =>
        simp only [discriminantOf, hd, hv]
        exact Outcome.all_ok (Value.Ok.int _ _)
  | unit => exact Outcome.all_undefined _ _
  | bool b => exact Outcome.all_undefined _ _
  | int ty x => exact Outcome.all_undefined _ _
  | array elems => exact Outcome.all_undefined _ _
  | ref p => exact Outcome.all_undefined _ _
  | dead => exact Outcome.all_undefined _ _

theorem constValue_ok (P : Pointer → Prop) : ∀ (c : Const), Value.Ok P (constValue c)
  | Const.unit => Value.Ok.unit
  | Const.bool b => Value.Ok.bool b
  | Const.int ty v => Value.Ok.int ty (Scalar.lit ty v)

theorem aggregateOf_ok (P : Pointer → Prop) (fields : List Value)
    (h : ∀ (v : Value), v ∈ fields → Value.Ok P v) :
    ∀ (kind : AggKind), Value.Ok P (aggregateOf kind fields)
  | AggKind.tuple => Value.Ok.adt 0 fields h
  | AggKind.adt _ variant => Value.Ok.adt variant fields h
  | AggKind.array => Value.Ok.array fields h

theorem mem_replicate' {α : Type} (v x : α) : ∀ (n : Nat), x ∈ List.replicate n v → x = v
  | 0, h => mem_nil_elim h
  | n + 1, h => (List.mem_cons.mp h).elim (fun e => e) (mem_replicate' v x n)

/-! Memory through a move. -/

theorem State.moveOut_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) :
    ∀ (p : Pointer), MemOk P (st.moveOut p)
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_ok hm { frame := frame, slot := slot, path := [] } Slot.moved True.intro
  | { frame := frame, slot := slot, path := e :: rest } =>
    stateOr_inv (MemOk P) st hm
      (st.write { frame := frame, slot := slot, path := e :: rest } Value.dead)
      (fun st' h =>
        State.write_ok hm { frame := frame, slot := slot, path := e :: rest } Value.dead
          Value.Ok.dead st' h)

/-! The invariant with the frames named. -/

/-- The state's frames are `ids`, and every pointer in its memory is one `PtrOk B ids`
allows. -/
def OkAt (B : FnId → Local → Prop) (ids : List (Nat × FnId)) (st : State) : Prop :=
  st.ids = ids ∧ MemOk (PtrOk B ids) st

theorem evalPlace_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {st : State}
    (hs : OkAt B ids st) (place : Place)
    (hb : B st.topFn place.base ∨ hasDeref place.proj = true) :
    Outcome.All (PtrOk B ids) (evalPlace st place) := by
  have e : st.ids = ids := And.left hs
  subst e
  intro q h
  exact evalProj_ptr (And.right hs) place.proj { frame := st.top, slot := place.base, path := [] }
    q (hb.elim (fun b => Or.inl (top_ptr B place.base st b)) (fun hd => Or.inr hd)) h

theorem evalOperand_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} (st : State)
    (hs : OkAt B ids st) :
    ∀ (o : Operand),
      Outcome.All (fun (r : Value × State) => Value.Ok (PtrOk B ids) r.1 ∧ OkAt B ids r.2)
        (evalOperand st o)
  | Operand.copy _ =>
    Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
      Outcome.all_bind (fun v h => State.read_ok (And.right hs) ptr v h) (fun _ hv =>
        Outcome.all_ok (And.intro hv hs)))
  | Operand.move _ =>
    Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
      Outcome.all_bind (fun v h => State.read_ok (And.right hs) ptr v h) (fun _ hv =>
        Outcome.all_ok
          (And.intro hv
            (And.intro ((State.moveOut_ids st ptr).trans (And.left hs))
              (State.moveOut_ok (And.right hs) ptr)))))
  | Operand.const c => Outcome.all_ok (And.intro (constValue_ok (PtrOk B ids) c) hs)

theorem evalOperands_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} :
    ∀ (os : List Operand) (st : State), OkAt B ids st →
      Outcome.All
        (fun (r : List Value × State) =>
          (∀ (v : Value), v ∈ r.1 → Value.Ok (PtrOk B ids) v) ∧ OkAt B ids r.2)
        (evalOperands st os)
  | [], _, hs => Outcome.all_ok (And.intro (fun _ hv => mem_nil_elim hv) hs)
  | o :: rest, st, hs =>
    Outcome.all_bind (evalOperand_okAt st hs o) (fun a ha =>
      Outcome.all_bind (evalOperands_okAt rest a.2 (And.right ha)) (fun b hb =>
        Outcome.all_ok
          (And.intro
            (fun v hv =>
              (List.mem_cons.mp hv).elim (fun e => Value.Ok.of_eq e (And.left ha))
                (And.left hb v))
            (And.right hb))))

theorem evalTwo_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} (st : State)
    (hs : OkAt B ids st) (l r : Operand) (f : Value → Value → Outcome Value)
    (hf : ∀ (x y : Value), Outcome.All (Value.Ok (PtrOk B ids)) (f x y)) :
    Outcome.All (fun (x : Value × State) => Value.Ok (PtrOk B ids) x.1 ∧ OkAt B ids x.2)
      (evalTwo st l r f) :=
  Outcome.all_bind (evalOperand_okAt st hs l) (fun a ha =>
    Outcome.all_bind (evalOperand_okAt a.2 (And.right ha) r) (fun b hb =>
      Outcome.all_bind (hf a.1 b.1) (fun _ hv => Outcome.all_ok (And.intro hv (And.right hb)))))

theorem evalOne_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} (st : State)
    (hs : OkAt B ids st) (o : Operand) (f : Value → Outcome Value)
    (hf : ∀ (x : Value), Value.Ok (PtrOk B ids) x → Outcome.All (Value.Ok (PtrOk B ids)) (f x)) :
    Outcome.All (fun (x : Value × State) => Value.Ok (PtrOk B ids) x.1 ∧ OkAt B ids x.2)
      (evalOne st o f) :=
  Outcome.all_bind (evalOperand_okAt st hs o) (fun a ha =>
    Outcome.all_bind (hf a.1 (And.left ha)) (fun _ hv =>
      Outcome.all_ok (And.intro hv (And.right ha))))

theorem evalAt_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} (st : State)
    (hs : OkAt B ids st) (p : Place) (f : Value → Outcome Value)
    (hf : ∀ (x : Value), Outcome.All (Value.Ok (PtrOk B ids)) (f x)) :
    Outcome.All (fun (x : Value × State) => Value.Ok (PtrOk B ids) x.1 ∧ OkAt B ids x.2)
      (evalAt st p f) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun v _ =>
      Outcome.all_bind (hf v) (fun _ hr => Outcome.all_ok (And.intro hr hs))))

/-- A `ref` is of a place whose base local the function may borrow, or of a place through a
dereference. -/
def RvalueFine (B : FnId → Local → Prop) (fn : FnId) : Rvalue → Prop
  | Rvalue.ref _ p => B fn p.base ∨ hasDeref p.proj = true
  | Rvalue.use _ => True
  | Rvalue.binary _ _ _ => True
  | Rvalue.checked _ _ _ => True
  | Rvalue.withOverflow _ _ _ => True
  | Rvalue.unary _ _ => True
  | Rvalue.cast _ _ => True
  | Rvalue.aggregate _ _ => True
  | Rvalue.replicate _ _ => True
  | Rvalue.discriminant _ _ => True
  | Rvalue.len _ => True

theorem evalRvalue_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)}
    (adts : List AdtDecl) (st : State) (hs : OkAt B ids st) (fn : FnId) (hfn : st.topFn = fn) :
    ∀ (rv : Rvalue), RvalueFine B fn rv →
      Outcome.All (fun (x : Value × State) => Value.Ok (PtrOk B ids) x.1 ∧ OkAt B ids x.2)
        (evalRvalue adts st rv)
  | Rvalue.use o, _ => evalOperand_okAt st hs o
  | Rvalue.binary op l r, _ => evalTwo_okAt st hs l r _ (binaryOp_ok (PtrOk B ids) false op)
  | Rvalue.checked op l r, _ => evalTwo_okAt st hs l r _ (binaryOp_ok (PtrOk B ids) true op)
  | Rvalue.withOverflow op l r, _ =>
    evalTwo_okAt st hs l r _ (overflowingOp_ok (PtrOk B ids) op)
  | Rvalue.unary op o, _ =>
    evalOne_okAt st hs o _ (fun x _ => unaryOp_ok (PtrOk B ids) op x)
  | Rvalue.cast o target, _ =>
    evalOne_okAt st hs o _ (fun x _ => castTo_ok (PtrOk B ids) target x)
  | Rvalue.aggregate kind fields, _ =>
    Outcome.all_bind (evalOperands_okAt fields st hs) (fun r hr =>
      Outcome.all_ok
        (And.intro (aggregateOf_ok (PtrOk B ids) r.1 (And.left hr) kind) (And.right hr)))
  | Rvalue.replicate o count, _ =>
    evalOne_okAt st hs o _ (fun x hx =>
      Outcome.all_ok
        (Value.Ok.array (List.replicate count x) (fun y hy =>
          Value.Ok.of_eq (mem_replicate' x y count hy) hx)))
  | Rvalue.discriminant adt p, _ =>
    evalAt_okAt st hs p _ (discriminantOf_ok (PtrOk B ids) adts adt)
  | Rvalue.ref _ p, hfine =>
    Outcome.all_bind
      (evalPlace_okAt hs p
        (hfine.elim (fun b => Or.inl (by rw [hfn]; exact b)) (fun hd => Or.inr hd)))
      (fun ptr hptr => Outcome.all_ok (And.intro (Value.Ok.ref ptr hptr) hs))
  | Rvalue.len p, _ => evalAt_okAt st hs p _ (lenOf_ok (PtrOk B ids))

def StmtFine (B : FnId → Local → Prop) (fn : FnId) : Statement → Prop
  | Statement.assign _ rv => RvalueFine B fn rv
  | Statement.storageLive _ => True
  | Statement.storageDead _ => True
  | Statement.nop => True

/-- A statement of a body that borrows only what `B` allows keeps the invariant. -/
theorem execStmt_okAt {B : FnId → Local → Prop} {ids : List (Nat × FnId)} (adts : List AdtDecl)
    (st : State) (hs : OkAt B ids st) (fn : FnId) (hfn : st.topFn = fn) :
    ∀ (s : Statement), StmtFine B fn s → Outcome.All (OkAt B ids) (execStmt adts st s)
  | Statement.assign _ rv, hfine =>
    Outcome.all_bind (evalRvalue_okAt adts st hs fn hfn rv hfine) (fun r hr =>
      Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
        (fun st' h =>
          And.intro
            ((State.write_ids r.2 ptr r.1 st' h).trans (And.left (And.right hr)))
            (State.write_ok (And.right (And.right hr)) ptr r.1 (And.left hr) st' h))))
  | Statement.storageLive _, _ => Outcome.all_ok hs
  | Statement.storageDead _, _ => Outcome.all_ok hs
  | Statement.nop, _ => Outcome.all_ok hs

end Blaze.Mir
