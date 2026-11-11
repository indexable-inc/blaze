import Blaze.Scalar

/-!
MIR, deeply embedded: the syntax of blaze's MIR (crates/hir-ty/src/mir.rs) for the first
subset, as data. `crates/hir-ty-lean/src/embed.rs` writes a function's `MirBody` as a term of
these types; `Blaze.Mir.Step` gives them their meaning.

What blaze's MIR has and this does not is refused by the encoder, by name (floats, raw
pointers, statics, closures, coroutines, unsizing, `Subslice`, inline assembly, ..). blaze's
MIR has no `SetDiscriminant` statement, so neither does this.
-/

namespace Blaze.Mir

abbrev Local := Nat
abbrev BlockId := Nat
abbrev FnId := Nat

/-- A type. A tuple type is declared like a struct (`adt`), so no type holds a list of
types. -/
inductive Ty where
  | unit
  | bool
  | int (ty : ScalarTy)
  /-- A struct, a tuple or an enum: `Program.adts` has its layout. -/
  | adt (id : Nat)
  | ref (mutable : Bool) (pointee : Ty)
  | slice (elem : Ty)
  | array (elem : Ty) (len : Nat)
  deriving DecidableEq, Repr

/-- The declared layout of a struct, a tuple or an enum: per variant, its discriminant and
its fields' types. A struct or a tuple has one variant, of discriminant 0. -/
structure AdtDecl where
  name : String
  /-- The type `Rvalue.discriminant` reads the discriminant as. -/
  discr : ScalarTy
  variants : List (Int × List Ty)
  deriving DecidableEq, Repr

inductive ProjElem where
  | deref
  | field (i : Nat)
  /-- The value as its variant `variant`; a `field` follows. -/
  | downcast (variant : Nat)
  /-- `base[i]` with `i` a local: panics out of bounds, as built MIR's index does. -/
  | index (i : Local)
  | constIndex (n : Nat)
  deriving DecidableEq, Repr

structure Place where
  base : Local
  proj : List ProjElem
  deriving DecidableEq, Repr

inductive Const where
  | unit
  | bool (b : Bool)
  /-- An integer by its value (not its bits): `Scalar.lit ty v`. -/
  | int (ty : ScalarTy) (v : Int)
  deriving DecidableEq, Repr

inductive Operand where
  | copy (p : Place)
  /-- Reads the place; a whole local is moved out of, and a read of it after is undefined. -/
  | move (p : Place)
  | const (c : Const)
  deriving DecidableEq, Repr

inductive BinOp where
  | add | sub | mul | div | rem
  | bitXor | bitAnd | bitOr
  | shl | shr
  | eq | lt | le | ne | ge | gt
  deriving DecidableEq, Repr

inductive UnOp where
  | not
  /-- `-x`, wrapping. -/
  | neg
  /-- `-x`, panicking on the least value: `Neg` under overflow checks. -/
  | negChecked
  deriving DecidableEq, Repr

inductive AggKind where
  | tuple
  | adt (id : Nat) (variant : Nat)
  | array
  deriving DecidableEq, Repr

inductive Rvalue where
  | use (o : Operand)
  /-- `BinaryOp`: `+ - * << >>` wrap; `/` and `%` panic on a zero divisor and on overflow. -/
  | binary (op : BinOp) (l r : Operand)
  /-- `CheckedBinaryOp` under overflow checks: `+ - * << >>` panic on overflow. -/
  | checked (op : BinOp) (l r : Operand)
  /-- Runtime MIR's `AddWithOverflow` and the like: the pair of the wrapped result and
  whether it overflowed. -/
  | withOverflow (op : BinOp) (l r : Operand)
  | unary (op : UnOp) (o : Operand)
  /-- `o as target`, from an integer or a `bool`. -/
  | cast (o : Operand) (target : ScalarTy)
  | aggregate (kind : AggKind) (fields : List Operand)
  /-- `[o; count]`. -/
  | replicate (o : Operand) (count : Nat)
  /-- The discriminant of the enum `adt` at `p`. -/
  | discriminant (adt : Nat) (p : Place)
  | ref (mutable : Bool) (p : Place)
  | len (p : Place)
  deriving DecidableEq, Repr

inductive Statement where
  | assign (p : Place) (rv : Rvalue)
  /-- Present, and does nothing in the first subset. -/
  | storageLive (l : Local)
  | storageDead (l : Local)
  | nop
  deriving DecidableEq, Repr

/-- A function of core the semantics gives a meaning itself, as the functional models do
(`lean/Blaze/Scalar.lean`, `Slice.lean`). -/
inductive Intrinsic where
  /-- Core's panic entry points. -/
  | panic
  | sliceLen
  | wrappingAdd | wrappingSub | wrappingMul
  | saturatingAdd | saturatingSub
  /-- The constructor function of a tuple struct or of a variant: its arguments are the
  fields. -/
  | construct (variant : Nat)
  deriving DecidableEq, Repr

inductive Callee where
  | fn (id : FnId)
  | intrinsic (i : Intrinsic)
  deriving DecidableEq, Repr

inductive Terminator where
  | goto (target : BlockId)
  /-- Each arm is the value tested for (the integer's value; 0 and 1 for a `bool`) and its
  block. -/
  | switchInt (discr : Operand) (arms : List (Int × BlockId)) (otherwise : BlockId)
  | call (callee : Callee) (args : List Operand) (dest : Place) (target : Option BlockId)
  | ret
  /-- Panics with `kind` unless `cond` is `expected`. -/
  | assert (cond : Operand) (expected : Bool) (kind : Error) (target : BlockId)
  /-- A drop of a type without drop glue: a `goto`. -/
  | drop (p : Place) (target : BlockId)
  | unreachable
  deriving DecidableEq, Repr

structure BasicBlock where
  stmts : List Statement
  terminator : Terminator
  deriving DecidableEq, Repr

/-- A function. Local 0 is the return slot, locals `1..args` the parameters; block 0 is the
entry. -/
structure Body where
  name : String
  args : Nat
  locals : List Ty
  blocks : List BasicBlock
  deriving DecidableEq, Repr

/-- A program: its types and its functions, each by its index. -/
structure Program where
  adts : List AdtDecl
  fns : List Body
  deriving DecidableEq, Repr

end Blaze.Mir
