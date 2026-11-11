import Blaze
import BlazeModel.blaze_graph.ledger.fnv1a_from

/-!
The model of `blaze_graph::ledger::fnv1a_from`, compiled: reads a line of decimal numbers (the
seed, then each byte) and prints what the model gives, `fail` or `div`. What
harness/v2/lean_differential.py runs against the function blaze built.
-/

open Blaze

def answer (line : String) : String :=
  let words := (line.splitOn " ").filter (fun word => !word.isEmpty)
  match words.mapM String.toNat? with
  | none => "error: not numbers"
  | some [] => "error: no seed"
  | some (seed :: bytes) =>
    match Slice.ofList? (bytes.map (fun (byte : Nat) => Scalar.lit ScalarTy.U8 (Int.ofNat byte))) with
    | none => "error: too many bytes"
    | some slice =>
      match blaze_graph.ledger.fnv1a_from (bytes.length + 1)
          (Scalar.lit ScalarTy.U64 (seed : Int)) slice with
      | Result.ok hash => toString hash.val
      | Result.fail _ => "fail"
      | Result.div => "div"

partial def answerAll (input : IO.FS.Stream) (output : IO.FS.Stream) : IO Unit := do
  let line ← input.getLine
  if line.isEmpty then
    return ()
  output.putStrLn (answer line.trimRight)
  answerAll input output

def main : IO Unit := do
  let input ← IO.getStdin
  let output ← IO.getStdout
  answerAll input output
