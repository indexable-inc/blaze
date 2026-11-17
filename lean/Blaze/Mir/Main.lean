import Blaze.Mir.Runner
import Blaze.Mir.Tests.Golden

/-!
`mir_run`: the semantics over the embedded golden bodies (`Blaze.Mir.Tests.golden`), reading
calls from standard input (`Blaze.Mir.Runner`). For a corpus the harness writes a module of
the same shape with its own table.
-/

def main : IO Unit := Blaze.Mir.runMain Blaze.Mir.Tests.golden
