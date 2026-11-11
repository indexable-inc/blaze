import Blaze.Mir.Tests.Golden

/-!
The semantics, run: each `example` is a run the kernel evaluates, so `lake build` is the
test. Every terminator, the three kinds of panic, undefined behaviour and running out of
fuel are here, on the embedded golden bodies and on a few bodies written for the test; then
the two proved passes on bodies they change.

If the elaborator's evaluation of a `decide` is too slow, `decide +kernel` gives the same
proof by the kernel alone (no axiom); `native_decide` is not allowed.
-/

namespace Blaze.Mir.Tests

open Blaze Blaze.Mir

set_option maxRecDepth 8192

def u32 (n : Int) : Value := Value.int ScalarTy.U32 (Scalar.lit ScalarTy.U32 n)

def isInt (ty : ScalarTy) (want : Int) : Outcome Value → Bool
  | Outcome.ok (Value.int ty' v) => decide (ty' = ty) && decide (v.val = want)
  | _ => false

def isUnit : Outcome Value → Bool
  | Outcome.ok Value.unit => true
  | _ => false

def isPanic (want : Error) : Outcome Value → Bool
  | Outcome.panic e => decide (e = want)
  | _ => false

def isUndefined (want : Undefined) : Outcome Value → Bool
  | Outcome.undefined why => decide (why = want)
  | _ => false

def isDiverge : Outcome Value → Bool
  | Outcome.diverge => true
  | _ => false

/-! Checked arithmetic and `ret`. -/

example : isInt ScalarTy.U32 42 (arith.add_one.mir.call 4 0 [] [u32 41]) = true := by decide

example :
    isPanic Error.arithmetic (arith.add_one.mir.call 4 0 [] [u32 4294967295]) = true := by
  decide

/-! `switchInt` on a `bool`, an early return. -/

example : isInt ScalarTy.U32 3 (early.clamp.mir.call 4 0 [] [u32 3, u32 7]) = true := by decide

example : isInt ScalarTy.U32 7 (early.clamp.mir.call 4 0 [] [u32 9, u32 7]) = true := by decide

/-! A loop (`goto`), and running out of fuel. -/

example : isInt ScalarTy.U32 3 (loops.count.mir.call 20 0 [] [u32 3]) = true := by decide

example : isDiverge (loops.count.mir.call 5 0 [] [u32 3]) = true := by decide

/-! An enum: `discriminant`, `switchInt` on it, a field of a variant. -/

example :
    isInt ScalarTy.U32 0 (enums.side.mir.call 4 0 [] [Value.adt 0 []]) = true := by decide

example :
    isInt ScalarTy.U32 7 (enums.side.mir.call 4 0 [] [Value.adt 1 [u32 7]]) = true := by
  decide

/-- A variant the enum does not declare is undefined, not a panic. -/
example :
    isUndefined Undefined.illFormed (enums.side.mir.call 4 0 [] [Value.adt 2 []]) = true := by
  decide

/-! A call of a function, twice. -/

example : isInt ScalarTy.U32 20 (calls.quadruple.mir.call 8 0 [] [u32 5]) = true := by decide

example :
    isPanic Error.arithmetic (calls.quadruple.mir.call 8 0 [] [u32 2147483648]) = true := by
  decide

/-! A slice behind a reference, and an index out of bounds. -/

def slot (elems : List Value) : List Slot := [Slot.init (Value.array elems)]

def firstRef : Value := Value.ref { frame := 0, slot := 0, path := [] }

example :
    isInt ScalarTy.U32 7 (slices.first.mir.call 4 0 (slot [u32 7, u32 8]) [firstRef]) = true := by
  decide

example : isPanic Error.bounds (slices.first.mir.call 4 0 (slot []) [firstRef]) = true := by
  decide

/-! A write through a `&mut` parameter. -/

example :
    isUnit (structs.bump.mir.call 4 0 [Slot.init (Value.adt 0 [u32 1, u32 9])] [firstRef]) =
      true := by
  decide

/-- A caller that lends a local to `bump` and reads the field it wrote. -/
def lend : Program :=
  { adts := structs.bump.mir.adts,
    fns :=
      ({ name := "lend", args := 0,
         locals := [Ty.int ScalarTy.U32, Ty.adt 0, Ty.ref true (Ty.adt 0), Ty.unit],
         blocks :=
           [{ stmts :=
                [Statement.storageLive 1,
                 Statement.assign (Place.mk 1 [])
                   (Rvalue.aggregate (AggKind.adt 0 0)
                     [Operand.const (Const.int ScalarTy.U32 1),
                      Operand.const (Const.int ScalarTy.U32 9)]),
                 Statement.assign (Place.mk 2 []) (Rvalue.ref true (Place.mk 1 []))],
              terminator :=
                Terminator.call (Callee.fn 1) [Operand.move (Place.mk 2 [])] (Place.mk 3 [])
                  (some 1) },
            { stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.copy (Place.mk 1 [ProjElem.field 0]))),
                 Statement.nop],
              terminator := Terminator.drop (Place.mk 1 []) 2 },
            { stmts := [Statement.storageDead 1], terminator := Terminator.ret }] } : Body) ::
        structs.bump.mir.fns }

example : isInt ScalarTy.U32 9 (lend.call 8 0 [] []) = true := by decide

/-! `assert`, runtime MIR's overflow pair, and `unreachable`. -/

/-- `fn f(x: u32, ok: bool) -> u32 { assert!(ok); x + 1 }` as runtime MIR has it. -/
def asserts : Program :=
  { adts := [],
    fns :=
      [{ name := "asserts", args := 2,
         locals := [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32, Ty.bool, Ty.adt 0],
         blocks :=
           [{ stmts := [],
              terminator := Terminator.assert (Operand.copy (Place.mk 2 [])) true Error.panic 1 },
            { stmts :=
                [Statement.assign (Place.mk 3 [])
                   (Rvalue.withOverflow BinOp.add (Operand.copy (Place.mk 1 []))
                     (Operand.const (Const.int ScalarTy.U32 1)))],
              terminator :=
                Terminator.assert (Operand.copy (Place.mk 3 [ProjElem.field 1])) false
                  Error.arithmetic 2 },
            { stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.move (Place.mk 3 [ProjElem.field 0])))],
              terminator := Terminator.ret }] }] }

example : isInt ScalarTy.U32 6 (asserts.call 8 0 [] [u32 5, Value.bool true]) = true := by
  decide

example : isPanic Error.panic (asserts.call 8 0 [] [u32 5, Value.bool false]) = true := by
  decide

example :
    isPanic Error.arithmetic (asserts.call 8 0 [] [u32 4294967295, Value.bool true]) = true := by
  decide

/-- A body of one block with `terminator`, two locals and no argument. -/
def one (stmts : List Statement) (terminator : Terminator) : Program :=
  { adts := [],
    fns :=
      [{ name := "one", args := 0, locals := [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32],
         blocks := [{ stmts := stmts, terminator := terminator }] }] }

example : isUndefined Undefined.unreachable ((one [] Terminator.unreachable).call 4 0 [] []) =
    true := by
  decide

/-! Undefined behaviour: a read of a local nothing assigned, and of one moved out of. -/

example :
    isUndefined Undefined.uninit
      ((one [Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.copy (Place.mk 1 [])))]
        Terminator.ret).call 4 0 [] []) = true := by
  decide

example :
    isUndefined Undefined.moved
      ((one
        [Statement.assign (Place.mk 1 []) (Rvalue.use (Operand.const (Const.int ScalarTy.U32 1))),
         Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.move (Place.mk 1 []))),
         Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.copy (Place.mk 1 [])))]
        Terminator.ret).call 4 0 [] []) = true := by
  decide

/-! The first function with a proof: FNV-1a of the byte `a` from the offset basis. -/

example :
    isInt ScalarTy.U64 12638187200555641996
      (blaze_graph.ledger.fnv1a_from.mir.call 16 0
        (slot [Value.int ScalarTy.U8 (Scalar.lit ScalarTy.U8 97)])
        [Value.int ScalarTy.U64 (Scalar.lit ScalarTy.U64 14695981039346656037), firstRef]) =
      true := by
  decide

/-! The passes, on bodies they change. -/

/-- A switch on a constant and an assert of a constant that holds. -/
def constant : Program :=
  { adts := [],
    fns :=
      [{ name := "constant", args := 0, locals := [Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts := [Statement.storageLive 0],
              terminator :=
                Terminator.switchInt (Operand.const (Const.bool true)) [(0, 2)] 1 },
            { stmts := [],
              terminator :=
                Terminator.assert (Operand.const (Const.bool false)) false Error.panic 3 },
            { stmts := [], terminator := Terminator.unreachable },
            { stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 4))),
                 Statement.storageDead 0],
              terminator := Terminator.ret }] }] }

/-- What `SimplifyConstCondition` makes of `constant`: both conditions are `goto`s. -/
def constantSimplified : Program :=
  { adts := [],
    fns :=
      [{ name := "constant", args := 0, locals := [Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts := [Statement.storageLive 0], terminator := Terminator.goto 1 },
            { stmts := [], terminator := Terminator.goto 3 },
            { stmts := [], terminator := Terminator.unreachable },
            { stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 4))),
                 Statement.storageDead 0],
              terminator := Terminator.ret }] }] }

example : Opt.simplifyConstCondition constant = constantSimplified := rfl

example : isInt ScalarTy.U32 4 (constant.call 8 0 [] []) = true := by decide

example : isInt ScalarTy.U32 4 ((Opt.simplifyConstCondition constant).call 8 0 [] []) = true := by
  decide

/-- `RemoveStorageMarkers` takes both markers of `constant` out and nothing else. -/
example :
    (Opt.removeStorageMarkers constant).fns.map
        (fun body => body.blocks.map (fun blk => blk.stmts.length)) = [[0, 0, 0, 1]] :=
  rfl

example : isInt ScalarTy.U32 4 ((Opt.removeStorageMarkers constant).call 8 0 [] []) = true := by
  decide

end Blaze.Mir.Tests
