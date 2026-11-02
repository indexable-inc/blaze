---
status: plan
summary: lane fastcompile's speed of light: the bound each component's work and the hardware set, the arithmetic, and how far the design is from it. An index to every component's bound, then the stencil patcher's cost per IR instruction and the library reader's.
date: 2026-10-29
---
# Lane fastcompile: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here, written by the
agent that owns it; each edits only its own section. The machine figures are those of
[link's speed of light](../link/speed-of-light.md#the-machine), unless a section says otherwise.

The stencil's two components are below. Every other component's bound, arithmetic and distance
from it are in its own note:

| idea | component | bound |
|---|---|---|
| 1 | lazy compilation (`blaze-lazy`) | [lazy.md](lazy.md#speed-of-light) |
| 2 | the share key (`hir-ty-share`) | [share.md](share.md#speed-of-light): \(T \approx 300 + 75 P_t + 135 P_c\) ns per options walked, about 95% of it |
| 3 | exact test selection (`blaze-testsel`) | [select.md](select.md#speed-of-light) |
| 4 | the stencil patcher and library (`blaze-stencil`) | below |
| 5 | hash-consing (`blaze-hashcons`) | [hashcons.md](hashcons.md#speed-of-light) |
| 6 | the overlay and the run channel (`blaze-inproc`) | [inproc.md](inproc.md#speed-of-light) |
| 7 | the dependency image (`blaze-premap`) | [premap.md](premap.md#speed-of-light) |
| 8 | the global cache (`blaze-global`) | [global.md](global.md#speed-of-light) |
| 9, 10 | semantic edits and declared intent (`blaze-intent`) | [intent.md](intent.md#speed-of-light) |
| 11, 12 | history and PGO/BOLT (`blaze-history`, `tools/pgo/`) | [sched.md](sched.md#speed-of-light) |

## The stencil patcher (`fastcompile-stencil-patch`)

The work of [stencil-patch.md](stencil-patch.md): from a Cranelift `ir::Function` to bytes,
per IR instruction. The irreducible work per instruction:

- **Read the instruction.** Its `InstructionData` (16 bytes) and its operands' and results'
  entries in the value columns (4 to 8 bytes each). The IR was just built, so it is in L1 or
  L2: about 1 ns.
- **Find the stencil.** One probe of the library's perfect index by the 4-byte shape key,
  then the stencil's row. A library is 200 to 300 KB per variant, so a cold function's
  lookups mostly hit L2 (about 4 ns each). Once a compile thread is warm, the shapes a crate
  uses most are in L1.
- **Copy the bytes.** About 20 bytes, one or two unaligned 16-byte moves: about 1 ns.
- **Patch the holes.** About 2 per stencil, each a read-modify-write of 4 bytes and one
  range check: about 1 ns each.
- **Output bandwidth.** About 20 bytes written per instruction. At 10 GB/s for one core that
  is 2 ns, and it overlaps with the rest.

The sum is **about 8 to 10 ns per instruction** in one fused pass with the library in L2.
The cost is in latency, not bandwidth: the index probe and the column reads are dependent
loads.

The design is **30 to 60 ns per instruction**: 3 to 6 times the bound, which misses the 10%
bar. The gap is in the structure, not in any one step:

- **Passes.** Scan, select, slots, edges, size and emit each walk the instructions: six
  traversals of the layout's linked list and six reads of each instruction's data, against
  one. Each costs about 2 to 5 ns per instruction.
- **Columns.** Every pass writes columns by value or instruction (`uses`, `last`, `slot`,
  `sels`), which the later passes read back. One fused pass would keep these in registers.
- **Library checks.** Selection asks the library whether each candidate shape exists
  (accumulator forms, then the base shape): up to three index probes per instruction where
  one would do with a shape mask per op.

To come within 10% of the bound: fuse scan, select and slots into one forward pass (the last
use of a block-local value needs a backward walk of the block, which is cheap while the
block is in L1), and fuse size into emit by reserving an upper bound from the instruction
count and the library's largest stencil. The size pass exists today so the buffer is exact
and never grows. `tests/speed.rs` (sibling `fastcompile-stencil-diff`) measures the real
figure on the corpus's IR. Until then these numbers are estimates from the operation counts
above.

## The library reader and the generator (`fastcompile-stencil-gen`)

**The lookup** ([stencil-gen.md](stencil-gen.md#the-reader)). For about 7,000 stencils the
pilots are about 2,700 × 2 bytes (5 KB, in L1), the slots 7,400 × 8 bytes (59 KB) and the
rows 7,000 × 16 bytes (112 KB), both in L2 once warm. `find` then `stencil` is three
multiplies (about 9 cycles, overlapping the first load), a pilot load (L1, 5 cycles), the
slot (L2, 14 cycles) and the row (L2, 14 cycles), all dependent: about 40 cycles, 10 ns at
4 GHz, and fewer when the shapes a crate uses most are in L1. No allocation, and bounds checks
only on slices of checked offsets. The bound is two dependent loads after the hash, the slot
and the code, if the slot held the row: about 25 cycles. The extra row load is kept because
the patcher selects first (storing the stencil's number) and copies later, so it reads the row
at copy time anyway, and `src/tests/stencil.rs` relies on numbers in the set's order. A patcher
that copied as it selected could read the row from the slot, if the slot held it, and come to
the bound.

**The generator** runs once per build of blaze. Its work is LLVM's codegen of about 21,000
small functions (three variants), each about 50 to 100 µs at -O2: 0.4 to 0.7 s per variant,
on three threads at once. Parsing the module text is about a tenth of that. Reading the
object, disassembling about 10 instructions per stencil (about 1 µs each) and writing the
library adds about 10 ms per variant. That is within 10% of LLVM's own codegen, the part
that cannot be avoided, and the bound for this design is the slowest variant's compile.
