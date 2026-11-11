import Blaze
import BlazeModel.arith.add_one
import BlazeModel.structs.bump
import BlazeModel.enums.side
import BlazeModel.loops.count
import BlazeModel.early.clamp
import BlazeModel.slices.first
import BlazeModel.calls.double
import BlazeModel.calls.quadruple
import BlazeModel.blaze_graph.ledger.fnv1a_from
import BlazeModel.arith.add_one_agrees
import BlazeModel.early.clamp_agrees
import BlazeModel.loops.count_agrees

/-!
Every model the golden tests of crates/hir-ty-lean hold the translator to, imported so that
`lake build` type-checks each one as it was emitted.
-/
