import Blaze
import Lean

/-!
The words a Rust name is not written as: every token of Lean's that reads as one component of
an identifier, in the environment the models are compiled in (`import Blaze`, which holds every
module a model imports).

`keywords.txt` beside this file is that list, one a line, sorted. The translator escapes
exactly these (`crates/hir-ty-lean/src/lean.rs` reads the same file), so a name Lean would read
as a keyword is written `«name»`. This file fails where the list is not what Lean says, and
prints what it must be: a Lean release with a new keyword, or a library module with new
syntax, fails here and not in a model.
-/

namespace Blaze.Tests.Keywords

open Lean

/-- Whether `token` reads as one component of an identifier: the translator's own test of a
plain name (`component` in `lean.rs`). A token that does not is no name Rust has. -/
def identLike (token : String) : Bool :=
  match token.toList with
  | [] => false
  | first :: rest =>
    (first.isAlpha || first == '_')
      && rest.all (fun c => c.isAlphanum || c == '_' || c == '\'')
      && token != "_"

/-- Lean's tokens that read as identifiers, sorted. -/
def tokens : CoreM (List String) := do
  let all := (Parser.getTokenTable (← getEnv)).findPrefix ""
  return (all.filter identLike).qsort (· < ·) |>.toList |>.eraseDups

/-- The list in the tree. -/
def checkedIn : List String :=
  ((include_str "keywords.txt").splitOn "\n").filter (· ≠ "")

#eval show CoreM Unit from do
  let want ← tokens
  unless want == checkedIn do
    throwError "lean/Blaze/Tests/keywords.txt is not Lean's tokens that read as identifiers. It must be:\n{String.intercalate "\n" want}"

end Blaze.Tests.Keywords
