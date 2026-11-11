import Blaze.Mir.State

/-!
The meaning of MIR: `step` runs the running frame's block (its statements, then its
terminator) and gives the next state or the value the entry function returned. Every
function here is total and structurally recursive, so the kernel evaluates it.

Integer arithmetic is `Blaze.Scalar`'s, the functional models' own: overflow checks, wrapping
and casts have one home. A panic is `Outcome.panic` with `Blaze.Error`'s kinds; what Rust
calls undefined behaviour, and what only an ill-typed or ill-formed program does, is
`Outcome.undefined`.
-/

namespace Blaze.Mir

def illTyped {α : Type} : Outcome α := Outcome.undefined Undefined.illTyped

/-! Constants and operators. -/

def constValue : Const → Value
  | Const.unit => Value.unit
  | Const.bool b => Value.bool b
  | Const.int ty v => Value.int ty (Scalar.lit ty v)

/-- `b` as a `ty`, when it is one. -/
def asTy (ty : ScalarTy) {ty' : ScalarTy} (b : Scalar ty') : Option (Scalar ty) :=
  if h : ty' = ty then some ⟨b.val, h ▸ b.hmin, h ▸ b.hmax⟩ else none

theorem asTy_self (ty : ScalarTy) (b : Scalar ty) : asTy ty b = some b := by
  unfold asTy
  exact dif_pos rfl

def sameOr {ty : ScalarTy} : Option (Scalar ty) → (Scalar ty → Outcome Value) → Outcome Value
  | some b, f => f b
  | none, _ => illTyped

/-- `f a b` when `b` is of `a`'s type. -/
def withSame {ty ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty')
    (f : Scalar ty → Scalar ty → Outcome Value) : Outcome Value :=
  sameOr (asTy ty b) (f a)

theorem withSame_self {ty : ScalarTy} (a b : Scalar ty)
    (f : Scalar ty → Scalar ty → Outcome Value) : withSame a b f = f a b := by
  unfold withSame
  rw [asTy_self]
  rfl

/-- A checked operation's result as a value: its failure is a panic. -/
def intResult {ty : ScalarTy} (r : Result (Scalar ty)) : Outcome Value :=
  (Outcome.ofResult r).bind (fun v => Outcome.ok (Value.int ty v))

/-- `BinaryOp` on two integers of one type. -/
def sameBinary (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) : Outcome Value :=
  match op with
  | BinOp.add => Outcome.ok (Value.int ty (Scalar.wrapping_add a b))
  | BinOp.sub => Outcome.ok (Value.int ty (Scalar.wrapping_sub a b))
  | BinOp.mul => Outcome.ok (Value.int ty (Scalar.wrapping_mul a b))
  | BinOp.div => intResult (Scalar.div a b)
  | BinOp.rem => intResult (Scalar.rem a b)
  | BinOp.bitXor => Outcome.ok (Value.int ty (Scalar.xor a b))
  | BinOp.bitAnd => Outcome.ok (Value.int ty (Scalar.and a b))
  | BinOp.bitOr => Outcome.ok (Value.int ty (Scalar.or a b))
  | BinOp.shl => Outcome.ok (Value.int ty (Scalar.wrapping_shl a b))
  | BinOp.shr => Outcome.ok (Value.int ty (Scalar.wrapping_shr a b))
  | BinOp.eq => Outcome.ok (Value.bool (Scalar.eq a b))
  | BinOp.lt => Outcome.ok (Value.bool (Scalar.lt a b))
  | BinOp.le => Outcome.ok (Value.bool (Scalar.le a b))
  | BinOp.ne => Outcome.ok (Value.bool (Scalar.ne a b))
  | BinOp.ge => Outcome.ok (Value.bool (Scalar.ge a b))
  | BinOp.gt => Outcome.ok (Value.bool (Scalar.gt a b))

/-- `CheckedBinaryOp` under overflow checks on two integers of one type. -/
def sameChecked (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) : Outcome Value :=
  match op with
  | BinOp.add => intResult (Scalar.add a b)
  | BinOp.sub => intResult (Scalar.sub a b)
  | BinOp.mul => intResult (Scalar.mul a b)
  | BinOp.shl => intResult (Scalar.shl a b)
  | BinOp.shr => intResult (Scalar.shr a b)
  | other => sameBinary other a b

/-- A shift's operands may differ in type; every other operator's are of one. -/
def intBinary (checked : Bool) (op : BinOp) {ty ty' : ScalarTy} (a : Scalar ty)
    (b : Scalar ty') : Outcome Value :=
  match checked, op with
  | false, BinOp.shl => Outcome.ok (Value.int ty (Scalar.wrapping_shl a b))
  | false, BinOp.shr => Outcome.ok (Value.int ty (Scalar.wrapping_shr a b))
  | true, BinOp.shl => intResult (Scalar.shl a b)
  | true, BinOp.shr => intResult (Scalar.shr a b)
  | false, other => withSame a b (sameBinary other)
  | true, other => withSame a b (sameChecked other)

def boolBinary (op : BinOp) (a b : Bool) : Outcome Value :=
  match op with
  | BinOp.eq => Outcome.ok (Value.bool (a == b))
  | BinOp.ne => Outcome.ok (Value.bool (a != b))
  | BinOp.bitAnd => Outcome.ok (Value.bool (a && b))
  | BinOp.bitOr => Outcome.ok (Value.bool (a || b))
  | BinOp.bitXor => Outcome.ok (Value.bool (a != b))
  | _ => illTyped

def binaryOp (checked : Bool) (op : BinOp) : Value → Value → Outcome Value
  | Value.int _ a, Value.int _ b => intBinary checked op a b
  | Value.bool a, Value.bool b => boolBinary op a b
  | _, _ => illTyped

/-- Whether `x` is a value of `ty`. -/
def inRange (ty : ScalarTy) (x : Int) : Bool := decide (ty.min ≤ x ∧ x ≤ ty.max)

/-- `(wrapped result, overflowed)`, a tuple. -/
def sameOverflowing (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) : Outcome Value :=
  match op with
  | BinOp.add =>
    Outcome.ok (Value.adt 0
      [Value.int ty (Scalar.wrapping_add a b), Value.bool (!inRange ty (a.val + b.val))])
  | BinOp.sub =>
    Outcome.ok (Value.adt 0
      [Value.int ty (Scalar.wrapping_sub a b), Value.bool (!inRange ty (a.val - b.val))])
  | BinOp.mul =>
    Outcome.ok (Value.adt 0
      [Value.int ty (Scalar.wrapping_mul a b), Value.bool (!inRange ty (a.val * b.val))])
  | _ => illTyped

def overflowingOp (op : BinOp) : Value → Value → Outcome Value
  | Value.int _ a, Value.int _ b => withSame a b (sameOverflowing op)
  | _, _ => illTyped

def unaryOp (op : UnOp) : Value → Outcome Value
  | Value.bool b =>
    match op with
    | UnOp.not => Outcome.ok (Value.bool (!b))
    | _ => illTyped
  | Value.int ty a =>
    match op with
    | UnOp.not => Outcome.ok (Value.int ty (Scalar.not a))
    | UnOp.neg => Outcome.ok (Value.int ty (Scalar.wrapping_neg a))
    | UnOp.negChecked => intResult (Scalar.neg a)
  | _ => illTyped

def castTo (target : ScalarTy) : Value → Outcome Value
  | Value.int _ a => Outcome.ok (Value.int target (Scalar.cast target a))
  | Value.bool b => Outcome.ok (Value.int target (Scalar.ofBool target b))
  | _ => illTyped

def aggregateOf (kind : AggKind) (fields : List Value) : Value :=
  match kind with
  | AggKind.tuple => Value.adt 0 fields
  | AggKind.adt _ variant => Value.adt variant fields
  | AggKind.array => Value.array fields

/-- The length of an array, as a `usize`. -/
def lenOf : Value → Outcome Value
  | Value.array elems =>
    Outcome.ok (Value.int ScalarTy.Usize (Scalar.lit ScalarTy.Usize (elems.length : Int)))
  | _ => illTyped

/-- The discriminant of a value of the enum `adt`, as the declaration gives it. -/
def discriminantOf (adts : List AdtDecl) (adt : Nat) : Value → Outcome Value
  | Value.adt variant _ =>
    match nth? adts adt with
    | none => Outcome.undefined Undefined.illFormed
    | some decl =>
      match nth? decl.variants variant with
      | none => Outcome.undefined Undefined.illFormed
      | some entry => Outcome.ok (Value.int decl.discr (Scalar.lit decl.discr entry.1))
  | _ => illTyped

/-! Places. -/

def asRef : Value → Outcome Pointer
  | Value.ref p => Outcome.ok p
  | _ => illTyped

/-- The pointer to element `n`, which panics when there is none. -/
def inBounds (p : Pointer) (n : Nat) : Option Value → Outcome Pointer
  | some _ => Outcome.ok (p.push (PathElem.index n))
  | none => Outcome.panic Error.bounds

def indexAt (p : Pointer) (n : Nat) : Value → Outcome Pointer
  | Value.array elems => inBounds p n (nth? elems n)
  | _ => illTyped

def indexBy (p : Pointer) (arr : Value) : Value → Outcome Pointer
  | Value.int _ i => indexAt p i.val.toNat arr
  | _ => illTyped

/-- One projection from the place `p`. -/
def projStep (st : State) (p : Pointer) : ProjElem → Outcome Pointer
  | ProjElem.deref => (st.read p).bind asRef
  | ProjElem.field i => Outcome.ok (p.push (PathElem.field i))
  | ProjElem.downcast variant => Outcome.ok (p.push (PathElem.downcast variant))
  | ProjElem.index l =>
    (st.read { frame := st.top, slot := l, path := [] }).bind (fun i =>
      (st.read p).bind (fun arr => indexBy p arr i))
  | ProjElem.constIndex n => (st.read p).bind (indexAt p n)

def evalProj (st : State) : Pointer → List ProjElem → Outcome Pointer
  | p, [] => Outcome.ok p
  | p, e :: rest => (projStep st p e).bind (fun q => evalProj st q rest)

/-- Where a place of the running frame is. -/
def evalPlace (st : State) (place : Place) : Outcome Pointer :=
  evalProj st { frame := st.top, slot := place.base, path := [] } place.proj

/-! Operands, rvalues, statements. An operand's value comes with the state after it, which
differs when it moves. -/

def evalOperand (st : State) : Operand → Outcome (Value × State)
  | Operand.copy p =>
    (evalPlace st p).bind (fun ptr => (st.read ptr).bind (fun v => Outcome.ok (v, st)))
  | Operand.move p =>
    (evalPlace st p).bind (fun ptr =>
      (st.read ptr).bind (fun v => Outcome.ok (v, st.moveOut ptr)))
  | Operand.const c => Outcome.ok (constValue c, st)

def evalOperands : State → List Operand → Outcome (List Value × State)
  | st, [] => Outcome.ok ([], st)
  | st, o :: rest =>
    (evalOperand st o).bind (fun r =>
      (evalOperands r.2 rest).bind (fun rs => Outcome.ok (r.1 :: rs.1, rs.2)))

/-- Two operands, left first, then `f` of their values. -/
def evalTwo (st : State) (l r : Operand) (f : Value → Value → Outcome Value) :
    Outcome (Value × State) :=
  (evalOperand st l).bind (fun a =>
    (evalOperand a.2 r).bind (fun b => (f a.1 b.1).bind (fun v => Outcome.ok (v, b.2))))

/-- One operand, then `f` of its value. -/
def evalOne (st : State) (o : Operand) (f : Value → Outcome Value) : Outcome (Value × State) :=
  (evalOperand st o).bind (fun a => (f a.1).bind (fun v => Outcome.ok (v, a.2)))

/-- The value at a place, then `f` of it. -/
def evalAt (st : State) (p : Place) (f : Value → Outcome Value) : Outcome (Value × State) :=
  (evalPlace st p).bind (fun ptr =>
    (st.read ptr).bind (fun v => (f v).bind (fun r => Outcome.ok (r, st))))

def evalRvalue (adts : List AdtDecl) (st : State) : Rvalue → Outcome (Value × State)
  | Rvalue.use o => evalOperand st o
  | Rvalue.binary op l r => evalTwo st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne st o (unaryOp op)
  | Rvalue.cast o target => evalOne st o (castTo target)
  | Rvalue.aggregate kind fields =>
    (evalOperands st fields).bind (fun r => Outcome.ok (aggregateOf kind r.1, r.2))
  | Rvalue.replicate o count =>
    evalOne st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt st p (discriminantOf adts adt)
  | Rvalue.ref _ p => (evalPlace st p).bind (fun ptr => Outcome.ok (Value.ref ptr, st))
  | Rvalue.len p => evalAt st p lenOf

/-- The rvalue is evaluated, then the place, then the write. -/
def execStmt (adts : List AdtDecl) (st : State) : Statement → Outcome State
  | Statement.assign p rv =>
    (evalRvalue adts st rv).bind (fun r =>
      (evalPlace r.2 p).bind (fun ptr => r.2.write ptr r.1))
  | Statement.storageLive _ => Outcome.ok st
  | Statement.storageDead _ => Outcome.ok st
  | Statement.nop => Outcome.ok st

/-! Terminators. -/

/-- What a step gives: the next state, or what the entry function returned. -/
inductive Next where
  | state (st : State)
  | done (v : Value)

/-- The running frame goes on at `target`. -/
def State.jump (st : State) (target : BlockId) : Outcome Next :=
  match st.stack with
  | [] => Outcome.undefined Undefined.illFormed
  | fr :: rest => Outcome.ok (Next.state { base := st.base, stack := fr.withBlock target :: rest })

def State.jump? (st : State) : Option BlockId → Outcome Next
  | some target => st.jump target
  | none => Outcome.undefined Undefined.illFormed

/-- What a `switchInt` tests of a value. -/
def switchKey : Value → Option Int
  | Value.bool true => some 1
  | Value.bool false => some 0
  | Value.int _ v => some v.val
  | _ => none

def switchTarget (key : Int) : List (Int × BlockId) → BlockId → BlockId
  | [], otherwise => otherwise
  | arm :: rest, otherwise => if key = arm.1 then arm.2 else switchTarget key rest otherwise

def switchOn (st : State) (arms : List (Int × BlockId)) (otherwise : BlockId) :
    Option Int → Outcome Next
  | some key => st.jump (switchTarget key arms otherwise)
  | none => illTyped

def assertOn (st : State) (expected : Bool) (kind : Error) (target : BlockId) :
    Value → Outcome Next
  | Value.bool b => if b = expected then st.jump target else Outcome.panic kind
  | _ => illTyped

/-- What a call needs of the function called. -/
structure FnSig where
  args : Nat
  locals : Nat

/-- A new frame for a call of `id` with `args`: the return slot and the locals start
uninitialized. -/
def State.enter (st : State) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) : Outcome Next :=
  if args.length = sig.args then
    Outcome.ok (Next.state
      { base := st.base,
        stack :=
          ({ fn := id, block := 0,
             locals :=
               Slot.uninit ::
                 (args.map Slot.init ++ List.replicate (sig.locals - 1 - sig.args) Slot.uninit),
             dest := dest, target := target } : Frame) :: st.stack })
  else Outcome.undefined Undefined.illFormed

/-- The running frame returns what its local 0 holds: to its caller's destination, or as the
run's answer when it is the entry function. -/
def State.ret (st : State) : Outcome Next :=
  match st.stack with
  | [] => Outcome.undefined Undefined.illFormed
  | fr :: rest =>
    match nth? fr.locals 0 with
    | none => Outcome.undefined Undefined.illFormed
    | some slot =>
      slot.load.bind (fun v =>
        match rest with
        | [] => Outcome.ok (Next.done v)
        | _ :: _ =>
          (State.write { base := st.base, stack := rest } fr.dest v).bind (fun st1 =>
            st1.jump? fr.target))

def saturating (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) : Outcome Value :=
  match op with
  | BinOp.add => Outcome.ok (Value.int ty (Scalar.saturating_add a b))
  | BinOp.sub => Outcome.ok (Value.int ty (Scalar.saturating_sub a b))
  | _ => illTyped

def saturatingOp (op : BinOp) : Value → Value → Outcome Value
  | Value.int _ a, Value.int _ b => withSame a b (saturating op)
  | _, _ => illTyped

def Intrinsic.eval (st : State) : Intrinsic → List Value → Outcome Value
  | Intrinsic.panic, _ => Outcome.panic Error.panic
  | Intrinsic.sliceLen, [Value.ref p] => (st.read p).bind lenOf
  | Intrinsic.wrappingAdd, [a, b] => binaryOp false BinOp.add a b
  | Intrinsic.wrappingSub, [a, b] => binaryOp false BinOp.sub a b
  | Intrinsic.wrappingMul, [a, b] => binaryOp false BinOp.mul a b
  | Intrinsic.saturatingAdd, [a, b] => saturatingOp BinOp.add a b
  | Intrinsic.saturatingSub, [a, b] => saturatingOp BinOp.sub a b
  | Intrinsic.construct variant, fields => Outcome.ok (Value.adt variant fields)
  | _, _ => illTyped

def callOn (sig : FnId → Option FnSig) (st : State) (callee : Callee) (args : List Value)
    (dest : Pointer) (target : Option BlockId) : Outcome Next :=
  match callee with
  | Callee.fn id =>
    match sig id with
    | some s => st.enter id s args dest target
    | none => Outcome.undefined Undefined.illFormed
  | Callee.intrinsic i =>
    (i.eval st args).bind (fun v => (st.write dest v).bind (fun st1 => st1.jump? target))

def execTerm (sig : FnId → Option FnSig) (st : State) : Terminator → Outcome Next
  | Terminator.goto target => st.jump target
  | Terminator.switchInt discr arms otherwise =>
    (evalOperand st discr).bind (fun r => switchOn r.2 arms otherwise (switchKey r.1))
  | Terminator.call callee args dest target =>
    (evalOperands st args).bind (fun r =>
      (evalPlace r.2 dest).bind (fun ptr => callOn sig r.2 callee r.1 ptr target))
  | Terminator.ret => st.ret
  | Terminator.assert cond expected kind target =>
    (evalOperand st cond).bind (fun r => assertOn r.2 expected kind target r.1)
  | Terminator.drop _ target => st.jump target
  | Terminator.unreachable => Outcome.undefined Undefined.unreachable

/-! Blocks and the step. -/

/-- The statements in order, then the terminator. -/
def execBlockFrom (adts : List AdtDecl) (sig : FnId → Option FnSig) :
    State → List Statement → Terminator → Outcome Next
  | st, [], t => execTerm sig st t
  | st, s :: rest, t => (execStmt adts st s).bind (fun st1 => execBlockFrom adts sig st1 rest t)

theorem execBlockFrom_cons (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (s : Statement) (rest : List Statement) (t : Terminator) :
    execBlockFrom adts sig st (s :: rest) t =
      (execStmt adts st s).bind (fun st1 => execBlockFrom adts sig st1 rest t) := rfl

def execBlock (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (blk : BasicBlock) : Outcome Next :=
  execBlockFrom adts sig st blk.stmts blk.terminator

def execBlock? (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State) :
    Option BasicBlock → Outcome Next
  | some blk => execBlock adts sig st blk
  | none => Outcome.undefined Undefined.illFormed

def Program.sig (prog : Program) (id : FnId) : Option FnSig :=
  (nth? prog.fns id).map (fun body => ({ args := body.args, locals := body.locals.length } : FnSig))

def blockOf (block : BlockId) : Option Body → Option BasicBlock
  | some body => nth? body.blocks block
  | none => none

def Program.block (prog : Program) (fn : FnId) (block : BlockId) : Option BasicBlock :=
  blockOf block (nth? prog.fns fn)

def stepFrame (prog : Program) (st : State) (fr : Frame) : Outcome Next :=
  execBlock? prog.adts prog.sig st (prog.block fr.fn fr.block)

/-- One step: the running frame's block. -/
def step (prog : Program) (st : State) : Outcome Next :=
  match st.stack with
  | [] => Outcome.undefined Undefined.illFormed
  | fr :: _ => stepFrame prog st fr

/-- `step` is a function, so a state has one next outcome: the semantics is deterministic by
construction. -/
theorem step_deterministic (prog : Program) (st : State) (a b : Outcome Next)
    (ha : step prog st = a) (hb : step prog st = b) : a = b :=
  ha.symm.trans hb

end Blaze.Mir
