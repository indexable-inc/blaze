import Blaze.Mir
import BlazeMir.arith.add_one
import BlazeMir.structs.bump
import BlazeMir.enums.side
import BlazeMir.loops.count
import BlazeMir.early.clamp
import BlazeMir.slices.first
import BlazeMir.calls.quadruple
import BlazeMir.blaze_graph.ledger.fnv1a_from

/-!
Every embedded body the golden tests of crates/hir-ty-lean hold the encoder to, imported so
that `lake build` type-checks each one as it was written; and the table of them that
`Blaze.Mir.Main` runs by name.
-/

namespace Blaze.Mir.Tests

def golden : List (String × Program) :=
  [("arith::add_one", arith.add_one.mir),
   ("structs::bump", structs.bump.mir),
   ("enums::side", enums.side.mir),
   ("loops::count", loops.count.mir),
   ("early::clamp", early.clamp.mir),
   ("slices::first", slices.first.mir),
   ("calls::quadruple", calls.quadruple.mir),
   ("blaze_graph::ledger::fnv1a_from", blaze_graph.ledger.fnv1a_from.mir)]

end Blaze.Mir.Tests
