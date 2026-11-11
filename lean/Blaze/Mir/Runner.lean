import Blaze.Mir.Run

/-!
The semantics as a program: reads calls from standard input, one a line, runs each and prints
its outcome in a fixed form. harness/v2/mir_semantics.py compares these lines with what
blaze's MIR evaluator and the code blaze built say.

A line is `<program> <function> <fuel> <argument>..`: the program by its name in the table
given to `runMain`, the function by its index in the program. An argument is `u8:255`,
`i32:-4`, `bool:true`, `unit`, or `&u8:1,2,3` (a shared reference to a slice, whose elements
go to frame 0). The answer is one of

    ok <value>              an integer in decimal, `true`, `false`, `()`, `{variant: fields}`,
                            `[elements]`
    panic <kind>            `panic`, `arithmetic`, `bounds`
    undefined <why>
    diverge                 out of fuel
    error: <what>           the line could not be read

The program is Lean source, the encoder's one output (`crates/hir-ty-lean/src/embed.rs`): the
table is compiled in, and nothing parses a second form of it.
-/

namespace Blaze.Mir

def parseTy : String → Option ScalarTy
  | "u8" => some ScalarTy.U8
  | "u16" => some ScalarTy.U16
  | "u32" => some ScalarTy.U32
  | "u64" => some ScalarTy.U64
  | "u128" => some ScalarTy.U128
  | "usize" => some ScalarTy.Usize
  | "i8" => some ScalarTy.I8
  | "i16" => some ScalarTy.I16
  | "i32" => some ScalarTy.I32
  | "i64" => some ScalarTy.I64
  | "i128" => some ScalarTy.I128
  | "isize" => some ScalarTy.Isize
  | _ => none

/-- An argument as written: a value, or the elements of a slice passed by reference. -/
inductive Arg where
  | value (v : Value)
  | slice (elems : List Value)

def intOf (ty : ScalarTy) (n : Int) : Value := Value.int ty (Scalar.lit ty n)

def parseInts (text : String) : Option (List Int) :=
  if text.isEmpty then some [] else (text.splitOn ",").mapM String.toInt?

def parseArg (token : String) : Option Arg :=
  match token.splitOn ":" with
  | ["unit"] => some (Arg.value Value.unit)
  | ["bool", "true"] => some (Arg.value (Value.bool true))
  | ["bool", "false"] => some (Arg.value (Value.bool false))
  | [ty, text] =>
    if ty.startsWith "&" then
      match parseTy (ty.drop 1), parseInts text with
      | some scalar, some ints => some (Arg.slice (ints.map (intOf scalar)))
      | _, _ => none
    else
      match parseTy ty, text.toInt? with
      | some scalar, some n => some (Arg.value (intOf scalar n))
      | _, _ => none
  | _ => none

/-- Frame 0's slots and the argument values: a slice goes to a slot, its argument is the
reference to it. -/
def placeArgs : List Arg → List Slot → List Value → List Slot × List Value
  | [], base, args => (base, args)
  | Arg.value v :: rest, base, args => placeArgs rest base (args ++ [v])
  | Arg.slice elems :: rest, base, args =>
    placeArgs rest (base ++ [Slot.init (Value.array elems)])
      (args ++ [Value.ref { frame := 0, slot := base.length, path := [] }])

/-- A value in the fixed form, to `depth` levels of nesting. -/
def renderValue : Nat → Value → String
  | _, Value.unit => "()"
  | _, Value.bool b => if b then "true" else "false"
  | _, Value.int _ v => toString v.val
  | _, Value.ref _ => "&"
  | _, Value.dead => "moved"
  | 0, _ => "..."
  | depth + 1, Value.adt variant fields =>
    "{" ++ toString variant ++ ": " ++ ", ".intercalate (fields.map (renderValue depth)) ++ "}"
  | depth + 1, Value.array elems =>
    "[" ++ ", ".intercalate (elems.map (renderValue depth)) ++ "]"

def renderError : Error → String
  | Error.panic => "panic"
  | Error.arithmetic => "arithmetic"
  | Error.bounds => "bounds"
  | Error.unreachable => "unreachable"

def renderUndefined : Undefined → String
  | Undefined.uninit => "uninit"
  | Undefined.moved => "moved"
  | Undefined.dangling => "dangling"
  | Undefined.illTyped => "ill-typed"
  | Undefined.illFormed => "ill-formed"
  | Undefined.unreachable => "unreachable"

def renderOutcome : Outcome Value → String
  | Outcome.ok v => "ok " ++ renderValue 64 v
  | Outcome.panic e => "panic " ++ renderError e
  | Outcome.undefined why => "undefined " ++ renderUndefined why
  | Outcome.diverge => "diverge"

def findProgram (programs : List (String × Program)) (name : String) : Option Program :=
  match programs.find? (fun entry => entry.1 == name) with
  | some entry => some entry.2
  | none => none

/-- The answer to one line. -/
def answer (programs : List (String × Program)) (line : String) : String :=
  match (line.splitOn " ").filter (fun word => !word.isEmpty) with
  | name :: fn :: fuel :: args =>
    match findProgram programs name, fn.toNat?, fuel.toNat?, args.mapM parseArg with
    | some prog, some id, some steps, some parsed =>
      let placed := placeArgs parsed [] []
      renderOutcome (prog.call steps id placed.1 placed.2)
    | none, _, _, _ => "error: no program `" ++ name ++ "`"
    | _, _, _, _ => "error: the function, the fuel or an argument is not as documented"
  | _ => "error: a line is `<program> <function> <fuel> <argument>..`"

/-- Answers up to `lines` lines of `input`; stops at its end. -/
def answerLines (programs : List (String × Program)) (input output : IO.FS.Stream) :
    Nat → IO Unit
  | 0 => pure ()
  | lines + 1 => do
    let line ← input.getLine
    if line.isEmpty then
      pure ()
    else
      output.putStrLn (answer programs line.trimRight)
      answerLines programs input output lines

/-- The most lines one run answers. -/
def mostLines : Nat := 1000000

def runMain (programs : List (String × Program)) : IO Unit := do
  let input ← IO.getStdin
  let output ← IO.getStdout
  answerLines programs input output mostLines

end Blaze.Mir
