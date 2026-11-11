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

/-- A move out of a part of a local leaves a hole there: a read of the part after is
undefined, and the rest of the local is still read. -/
def movePart (read : Nat) : Program :=
  { adts := [],
    fns :=
      [{ name := "movePart", args := 0,
         locals := [Ty.int ScalarTy.U32, Ty.adt 0, Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 1 [])
                   (Rvalue.aggregate AggKind.tuple
                     [Operand.const (Const.int ScalarTy.U32 3),
                      Operand.const (Const.int ScalarTy.U32 4)]),
                 Statement.assign (Place.mk 2 [])
                   (Rvalue.use (Operand.move (Place.mk 1 [ProjElem.field 0]))),
                 Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.copy (Place.mk 1 [ProjElem.field read])))],
              terminator := Terminator.ret }] }] }

example : isUndefined Undefined.moved ((movePart 0).call 4 0 [] []) = true := by decide

example : isInt ScalarTy.U32 4 ((movePart 1).call 4 0 [] []) = true := by decide

/-- A reference into a frame that returned is dangling for the rest of the run: the second
call's frame has an id of its own, so it is not taken for the first's. -/
def dangle : Program :=
  { adts := [],
    fns :=
      [{ name := "dangle", args := 0,
         locals :=
           [Ty.int ScalarTy.U32, Ty.ref false (Ty.int ScalarTy.U32),
            Ty.ref false (Ty.int ScalarTy.U32)],
         blocks :=
           [{ stmts := [],
              terminator := Terminator.call (Callee.fn 1) [] (Place.mk 1 []) (some 1) },
            { stmts := [],
              terminator := Terminator.call (Callee.fn 1) [] (Place.mk 2 []) (some 2) },
            { stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.copy (Place.mk 1 [ProjElem.deref])))],
              terminator := Terminator.ret }] },
       { name := "escape", args := 0,
         locals := [Ty.ref false (Ty.int ScalarTy.U32), Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 1 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 5))),
                 Statement.assign (Place.mk 0 []) (Rvalue.ref false (Place.mk 1 []))],
              terminator := Terminator.ret }] }] }

example : isUndefined Undefined.dangling (dangle.call 8 0 [] []) = true := by decide

/-- Two `&mut` to one local, both live: the model has no aliasing rule, so a write through
one is read through the other (7 here), where Rust says undefined behaviour. No program the
borrow checker accepts does this, and the encoder embeds only such programs: every theorem
about a Rust program has that as its hypothesis. -/
def aliased : Program :=
  { adts := [],
    fns :=
      [{ name := "aliased", args := 0,
         locals :=
           [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32, Ty.ref true (Ty.int ScalarTy.U32),
            Ty.ref true (Ty.int ScalarTy.U32)],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 1 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 1))),
                 Statement.assign (Place.mk 2 []) (Rvalue.ref true (Place.mk 1 [])),
                 Statement.assign (Place.mk 3 []) (Rvalue.ref true (Place.mk 1 [])),
                 Statement.assign (Place.mk 2 [ProjElem.deref])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 7))),
                 Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.copy (Place.mk 3 [ProjElem.deref])))],
              terminator := Terminator.ret }] }] }

example : isInt ScalarTy.U32 7 (aliased.call 4 0 [] []) = true := by decide

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

/-! Constant folding: an operation on constants is its value; one that panics stays. -/

def c32 (n : Int) : Operand := Operand.const (Const.int ScalarTy.U32 n)

example :
    Opt.foldRvalue (Rvalue.binary BinOp.add (c32 2) (c32 3)) = Rvalue.use (c32 5) := by
  decide

example :
    Opt.foldRvalue (Rvalue.checked BinOp.add (c32 4294967295) (c32 1)) =
      Rvalue.checked BinOp.add (c32 4294967295) (c32 1) := by
  decide

example :
    Opt.foldRvalue (Rvalue.binary BinOp.lt (c32 2) (c32 3)) =
      Rvalue.use (Operand.const (Const.bool true)) := by
  decide

example :
    Opt.foldRvalue (Rvalue.cast (c32 300) ScalarTy.U8) =
      Rvalue.use (Operand.const (Const.int ScalarTy.U8 44)) := by
  decide

/-! The validator: `constant` and what `SimplifyConstCondition` makes of it have one normal
form; a program that returns another constant does not. -/

example : Opt.validate constant constantSimplified = true := by decide

example : Opt.validate constant (one [] Terminator.ret) = false := by decide

/-! The CFG passes, on `constantSimplified`: block 1 is an empty `goto 3`, block 2 is dead.

Goto threading sends block 0 past block 1; the call gives the same value. -/

example :
    (Opt.threadGotos constantSimplified).fns.map
        (fun body => body.blocks.map (fun blk => blk.terminator)) =
      [[Terminator.goto 3, Terminator.goto 3, Terminator.unreachable, Terminator.ret]] := by
  decide

example : isInt ScalarTy.U32 4 ((Opt.threadGotos constantSimplified).call 8 0 [] []) = true := by
  decide

/-- The threaded program is done in two steps where the program takes three. -/
example : isInt ScalarTy.U32 4 ((Opt.threadGotos constantSimplified).call 2 0 [] []) = true := by
  decide

example : isDiverge (constantSimplified.call 2 0 [] []) = true := by decide

/-- Two rounds leave what one round made: `constantSimplified` has no chain of two. -/
example : Opt.threadGotosN 2 constantSimplified = Opt.threadGotos constantSimplified := by decide

/-! Dead-block removal keeps what block 0 reaches, in its old order, and renames the
targets. The check its theorem takes is evaluated, and the theorem is applied. -/

example : Opt.reachableOrders constantSimplified 0 = [0, 1, 3] := by decide

example : Opt.checkDead constantSimplified = true := by decide

example :
    (Opt.removeDeadBlocks constantSimplified).fns.map
        (fun body => body.blocks.map (fun blk => blk.terminator)) =
      [[Terminator.goto 1, Terminator.goto 2, Terminator.ret]] := by
  decide

example :
    (Opt.removeDeadBlocks constantSimplified).call 8 0 [] [] =
      constantSimplified.call 8 0 [] [] :=
  Opt.removeDeadBlocks_call constantSimplified (by decide) 8 0 [] []

example : isInt ScalarTy.U32 4 ((Opt.removeDeadBlocks constantSimplified).call 8 0 [] []) = true := by
  decide

/-- Threading, then dead-block removal: two blocks are left. -/
example :
    (Opt.removeDeadBlocks (Opt.threadGotos constantSimplified)).fns.map
        (fun body => body.blocks.length) = [2] := by
  decide

/-! A permutation of the blocks: the entry block stays first, every block is listed. -/

def shuffled : FnId → List BlockId := fun _ => [0, 3, 2, 1]

example : Opt.checkPermutation shuffled constantSimplified = true := by decide

/-- An order that misses a block is refused. -/
example : Opt.checkPermutation (fun _ => [0, 1, 2]) constantSimplified = false := by decide

/-- So is one that does not start at the entry block. -/
example : Opt.checkPermutation (fun _ => [1, 0, 2, 3]) constantSimplified = false := by decide

example :
    (Opt.renumber shuffled constantSimplified).fns.map
        (fun body => body.blocks.map (fun blk => blk.terminator)) =
      [[Terminator.goto 3, Terminator.ret, Terminator.unreachable, Terminator.goto 1]] := by
  decide

example :
    (Opt.renumber shuffled constantSimplified).call 8 0 [] [] =
      constantSimplified.call 8 0 [] [] :=
  Opt.permuteBlocks_call shuffled constantSimplified (by decide) 8 0 [] []

example : isInt ScalarTy.U32 4 ((Opt.renumber shuffled constantSimplified).call 8 0 [] []) = true := by
  decide

/-! A `switchInt` whose arms all go to one block is a `goto` to it; one whose arms differ is
left. -/

def sameArms (other : BlockId) : Program :=
  { adts := [],
    fns :=
      [{ name := "sameArms", args := 1, locals := [Ty.int ScalarTy.U32, Ty.bool],
         blocks :=
           [{ stmts := [],
              terminator := Terminator.switchInt (Operand.copy (Place.mk 1 [])) [(0, 1)] other },
            { stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 7)))],
              terminator := Terminator.ret },
            { stmts := [], terminator := Terminator.unreachable }] }] }

example :
    (Opt.switchToGoto (sameArms 1)).fns.map
        (fun body => body.blocks.map (fun blk => blk.terminator)) =
      [[Terminator.goto 1, Terminator.ret, Terminator.unreachable]] := by
  decide

example : Opt.switchToGoto (sameArms 2) = sameArms 2 := by decide

example :
    isInt ScalarTy.U32 7 ((Opt.switchToGoto (sameArms 1)).call 4 0 [] [Value.bool true]) =
      true := by
  decide

/-! Merging: block 1 has one predecessor and goes into block 0; block 3 has one and goes
into block 1. The merged program is done in two steps. -/

example :
    (Opt.mergeSuccessors constantSimplified).fns.map
        (fun body => body.blocks.map (fun blk => (blk.stmts.length, blk.terminator))) =
      [[(1, Terminator.goto 3), (2, Terminator.ret), (0, Terminator.unreachable),
        (2, Terminator.ret)]] := by
  decide

example :
    isInt ScalarTy.U32 4 ((Opt.mergeSuccessors constantSimplified).call 2 0 [] []) = true := by
  decide

/-! Threading through a call's return target: the caller of `bump` returns to an empty block
that only jumps on, and goes past it. -/

def callThenHop : Program :=
  { adts := [],
    fns :=
      [{ name := "caller", args := 0, locals := [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts := [],
              terminator := Terminator.call (Callee.fn 1) [] (Place.mk 1 []) (some 1) },
            { stmts := [], terminator := Terminator.goto 2 },
            { stmts :=
                [Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.copy (Place.mk 1 [])))],
              terminator := Terminator.ret }] },
       { name := "seven", args := 0, locals := [Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 7)))],
              terminator := Terminator.ret }] }] }

example :
    (Opt.threadAll callThenHop).block 0 0 =
      some { stmts := [],
             terminator := Terminator.call (Callee.fn 1) [] (Place.mk 1 []) (some 2) } := by
  decide

example : isInt ScalarTy.U32 7 (callThenHop.call 4 0 [] []) = true := by decide

example : isDiverge (callThenHop.call 3 0 [] []) = true := by decide

/-- The threaded program is done in three steps where the program takes four. -/
example : isInt ScalarTy.U32 7 ((Opt.threadAll callThenHop).call 3 0 [] []) = true := by decide

/-! `SimplifyCfg` composed: one round leaves `constantSimplified` one block, the three
statements and the return, and its call is done in one step. -/

example :
    (Opt.simplifyCfg 1 constantSimplified).fns.map
        (fun body => body.blocks.map (fun blk => (blk.stmts.length, blk.terminator))) =
      [[(3, Terminator.ret)]] := by
  decide

example : isInt ScalarTy.U32 4 ((Opt.simplifyCfg 1 constantSimplified).call 1 0 [] []) = true := by
  decide

/-! A store overwritten in its own block, before its local is mentioned, goes. -/

def storeTwice (read : Bool) : Program :=
  { adts := [],
    fns :=
      [{ name := "storeTwice", args := 0, locals := [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 1 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 5))),
                 (if read then
                   Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.copy (Place.mk 1 [])))
                 else Statement.nop),
                 Statement.assign (Place.mk 1 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 7))),
                 Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.copy (Place.mk 1 [])))],
              terminator := Terminator.ret }] }] }

example : Opt.checkBorrows Opt.refsOf (storeTwice false) = true := by decide

example :
    (Opt.removeOverwritten Opt.refsOf (storeTwice false)).fns.map
        (fun body => body.blocks.map (fun blk => blk.stmts.map Opt.isNop)) =
      [[[true, true, false, false]]] := by
  decide

/-- With the first store read before the second, it stays; the store to local 0 that the last
statement overwrites goes instead. -/
example :
    (Opt.removeOverwritten Opt.refsOf (storeTwice true)).fns.map
        (fun body => body.blocks.map (fun blk => blk.stmts.map Opt.isNop)) =
      [[[false, true, false, false]]] := by
  decide

example :
    isInt ScalarTy.U32 7
        ((Opt.removeOverwritten Opt.refsOf (storeTwice true)).call 2 0 [] []) = true := by
  decide

example :
    isInt ScalarTy.U32 7
        ((Opt.removeOverwritten Opt.refsOf (storeTwice false)).call 2 0 [] []) = true := by
  decide

/-! The witness validator: a script of proved rewrites takes `constantSimplified` to one
block; the same script does not take it to another program, and an empty script takes a
program only to itself. -/

def oneBlock : Program :=
  { adts := [],
    fns :=
      [{ name := "constant", args := 0, locals := [Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.storageLive 0,
                 Statement.assign (Place.mk 0 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 4))),
                 Statement.storageDead 0],
              terminator := Terminator.ret }] }] }

def cfgScript : List Opt.Rewrite :=
  [Opt.Rewrite.threadAll, Opt.Rewrite.removeDeadBlocks, Opt.Rewrite.mergeSuccessors,
   Opt.Rewrite.removeDeadBlocks]

example : Opt.validateWith constantSimplified oneBlock cfgScript = true := by decide

example : Opt.validateWith constantSimplified constantSimplified cfgScript = false := by decide

example : Opt.validateWith constantSimplified oneBlock [] = false := by decide

example : isInt ScalarTy.U32 4 (oneBlock.call 1 0 [] []) = true := by decide

/-- A script with `removeOverwritten` in it: the store to local 1 that its block overwrites
goes, then the `nop`s. -/
example :
    Opt.validateWith (storeTwice false)
        (Opt.removeNops (Opt.removeOverwritten Opt.refsOf (storeTwice false)))
        [Opt.Rewrite.removeOverwritten, Opt.Rewrite.removeNops] = true := by
  decide

example : Opt.allPlain cfgScript = true := by decide

/-! Symbolic equality: a use of a copy replaced by the local it copies (copy propagation of
a use, the copy kept) gives one symbolic form; another constant does not. -/

def viaCopy (use : Local) (k : Int) : Program :=
  { adts := [],
    fns :=
      [{ name := "viaCopy", args := 1,
         locals := [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32, Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 2 []) (Rvalue.use (Operand.copy (Place.mk 1 []))),
                 Statement.assign (Place.mk 0 [])
                   (Rvalue.checked BinOp.add (Operand.copy (Place.mk use []))
                     (Operand.const (Const.int ScalarTy.U32 k)))],
              terminator := Terminator.ret }] }] }

example : symEq (viaCopy 2 1) (viaCopy 1 1) = true := by decide

example : symEq (viaCopy 2 1) (viaCopy 1 2) = false := by decide

example : decide (viaCopy 2 1 = viaCopy 1 1) = false := by decide

example : Opt.validateSym (viaCopy 2 1) (viaCopy 1 1) [] = true := by decide

example : isInt ScalarTy.U32 6 ((viaCopy 1 1).call 1 0 [] [u32 5]) = true := by decide

/-! Liveness as a certificate. A store dead across blocks: local 1 is stored in block 0 and
stored again in block 1 before it is read. The certificate lists the locals live before
each statement and before each terminator; the check accepts it, the pass removes the first
store, and a certificate that calls local 1 dead where it is read is refused. -/

def storeAcross : Program :=
  { adts := [],
    fns :=
      [{ name := "storeAcross", args := 0, locals := [Ty.int ScalarTy.U32, Ty.int ScalarTy.U32],
         blocks :=
           [{ stmts :=
                [Statement.assign (Place.mk 1 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 5)))],
              terminator := Terminator.goto 1 },
            { stmts :=
                [Statement.assign (Place.mk 1 [])
                   (Rvalue.use (Operand.const (Const.int ScalarTy.U32 7))),
                 Statement.assign (Place.mk 0 []) (Rvalue.use (Operand.copy (Place.mk 1 [])))],
              terminator := Terminator.ret }] }] }

def liveAcross : List (List (List (List Local))) := [[[[], []], [[], [1], [0]]]]

example : Opt.checkLiveness storeAcross liveAcross = true := by decide

/-- Local 1 called dead before the statement that reads it. -/
example : Opt.checkLiveness storeAcross [[[[], []], [[], [], [0]]]] = false := by decide

/-- Local 0 called dead before the `ret` that returns it. -/
example : Opt.checkLiveness storeAcross [[[[], []], [[], [1], []]]] = false := by decide

example :
    (Opt.removeDead liveAcross Opt.refsOf storeAcross).fns.map
        (fun body => body.blocks.map (fun blk => blk.stmts.map Opt.isNop)) =
      [[[true], [false, false]]] := by
  decide

example :
    isInt ScalarTy.U32 7 ((Opt.removeDead liveAcross Opt.refsOf storeAcross).call 4 0 [] []) =
      true := by
  decide

/-- The pass as a rewrite of the witness validator: the certificate is the script's data. -/
example :
    Opt.validateWith storeAcross (Opt.removeDead liveAcross Opt.refsOf storeAcross)
        [Opt.Rewrite.removeDead liveAcross] = true := by
  decide

end Blaze.Mir.Tests
