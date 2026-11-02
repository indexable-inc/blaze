---
status: plan
summary: blaze v2 lane fastcompile, child fastcompile-stencil-patch of stencil.md: the patcher in crates/blaze-stencil (patch.rs, patch/, buf.rs, code.rs, enc.rs), its passes (scan, select, slots, frame, edges, size, emit, finish) and the columns each writes, the slot area's layout with the glue's scratch and the relay slots, how edges, fusions, accumulators, traps and constants are written, what is an error, the tests against a mock library and glue, and where it is faster or slower than Xu and Kjolstad's copy-and-patch, CPython's JIT, TPDE and Cranelift's fastalloc.
date: 2026-10-29
---
# fastcompile: the patcher (`blaze-stencil`'s `patch`)

Part of [stencil.md](stencil.md): one pass from a Cranelift `ir::Function` to machine code by
copying the library's stencils and filling their holes, with `arch::Glue` for the rest
(prologue, calls, returns, traps the glue takes, relays, unwind). The code is
`crates/blaze-stencil/src/patch.rs` and `patch/`; `buf.rs` is the one output buffer,
`enc.rs` the field encodings, `code.rs` the result. The speed bound is in
[speed-of-light.md](speed-of-light.md).

## The passes

Each pass writes columns of `patch::Plan`, flat `Vec`s indexed by `ir::Inst`, `ir::Value`,
`ir::Block` or `ir::StackSlot`. `patch::compile_in` runs the passes as one blaze-mem task
(`frame::task(Tag::Codegen, ..)`): the plan, the buffer (`Buf<'f>`) and the side tables
(`Side<'f>`) are vectors on the task's frame, so growing one never calls the global allocator
and the frame's reset frees them all. The glue's own temporaries (Windows unwind info) go on
the same frame through `Cx::temp`. The result is a `CodeRef` borrowed from the frame, handed
to the caller's sink; `Code::copy_from` refills a reused `Code` from it, so a warm compile
into a reused `Code` allocates nothing (the tests
`a_warm_compile_into_a_reused_code_allocates_nothing` here, and the ones through the x86-64
and AArch64 glue, count it). Relocations are sorted by an in-place insertion sort
(`buf::sort_by_key_in_place`) where a stable sort would take a buffer.

1. **scan** (`patch/scan.rs`): layout order of instructions and blocks, use counts, the last
   use of each value read only in its own block (`slots::GLOBAL` otherwise), constants
   (`iconst`, `f16const`..`f64const`, `vconst`), and pure instructions nothing uses (dead).
2. **select** (`patch/select.rs`): each instruction becomes `Kind::Skip` (nop, dead, a
   constant every use folds), `Glue` (`A::takes`), `Jump`, `Fused` (an `icmp` folded into the
   next instruction), or `Stencil` with its `Shape`. Operands that are constants fitting a
   shape's immediate are folded. `BrIcmp` and `SelectIcmp` fuse an `icmp` that only the next
   instruction reads. Every choice asks the library (`slots::has`), so a library without a
   shape falls to the next one that exists, and the base shape (slots only) always does. A
   construct no stencil and no glue takes is `Error::at(inst, ...)` naming the opcode and
   type.
3. **slots** (`patch/slots.rs`):
   - Accumulators first: a result read by the next stencil of its block at a position whose
     shape takes an accumulator is `Dst::Acc` (one use) or `Dst::Both` (more), the reader
     `Src::Acc`, when both shapes are in the library. Glue and jumps end a chain.
   - Global slots (block parameters, values read across blocks) are laid out first, by bump,
     and never shared.
   - Then a linear scan per block: results are given slots before the instruction's operands
     are freed, so a result never shares a slot with an operand. Free lists per size (8, 16)
     are kept in the plan for their allocations.
   - Stack slots follow the value slots at their alignment. Dynamic stack slots are an error.
4. **frame** (`A::frame`): the glue lays out the frame around the area
   (`Plan::area`: size rounded to 16, alignment at least 16).
5. **edges** (`emit::edges`): each branch's successors as `Edge`s. An edge has code of its
   own (`own`) when it is a landing pad, a back edge (to a block at or before its branch in
   layout order: it calls `Glue::count_back_edge`) or moves block arguments. A `BrIcmp` whose
   taken edge is the code that follows is complemented (`Sel::swapped`), so it falls through.
6. **size** (`emit::size`): each stencil's index is found once, and the bytes, labels,
   fixups and lines are added up (stencils, `A::bound` per glue instruction, `bound_fixed`
   per prologue, landing, relay, back-edge count and stub, edges' moves, constants). The
   buffer is reserved once.
7. **emit** (`emit::function`): the prologue at offset 0, then block by block: the block's
   label, each instruction (a line row when its source location changes; a stencil, glue, or
   a jump that is dropped when its target follows), then the block's own edges.
   - A stencil is copied whole, or without its `tail` (the jump to `Next`) when the next code
     follows. Holes in a dropped tail are skipped.
   - Slot holes get frame offsets; far ones (at or past `A::NEAR[size]`) go through relay
     slots, copied by `Glue::relay` before (operands) or after (results) the stencil.
   - A result nothing reads still has to go somewhere: it writes a glue scratch slot.
   - Each memory access the stencil lists (`Stencil::faults`) in the copied bytes is a trap
     site in `Code::traps`, with the instruction's `MemFlags` trap code (none for `notrap`).
   - `Target`, `Trap`, `TrapOverflow`, `Const` and a kept `Next` are label fixups; `Rodata`
     holes name a 16-byte library entry, placed once.
   - Edge moves are a parallel move: a move whose target no other move still reads goes
     first; a cycle is broken through scratch slot 0. Constants as block arguments are
     materialized with an `Iconst`/`Fconst` stencil.
8. **finish**: trap stubs (one per trap code, in the order first used, each recorded in
   `Code::traps` with its offset; a stub traps at its first instruction), then, 16-aligned
   with `A::NOP`, the constants (one per pool handle or immediate) and the library rodata
   entries. Fixups are resolved, the glue closes the unwind rows, call sites get their
   handlers, and stack-slot offsets are given relative to `Glue::slot_base`.

## The area

From the frame register, upward:

| offset | what |
|---|---|
| 0, 16 | the glue's two 16-byte scratch slots (`Plan::scratch`) |
| 32 to 112 | five relay slots (three operands, two results), only when the area reaches past `A::NEAR[0]` |
| `base` | value slots: globals, then locals |
| after | stack slots at their alignment |

## Conventions the siblings program against

These are in the report at `/tmp/blaze-lanes/v2-fastcompile-stencil-patch.md` and in the
docs of `arch.rs`. The ones that changed while the patcher was written:

- Selection asks the library, not `Shape::exists`, so a mock or a pruned library works.
- A result nothing reads writes scratch slot 0 (result 0) or 16 (result 1).
- Relay slot `n` is at `32 + 16n`: 0 to 2 for operands, 3 and 4 for results.
- A `try_call`'s normal edge is written right after its glue; handler edges after the
  block (label, `Glue::landing`, moves, jump).
- Rodata holes hold `(entry << 8) | adjust` and name 16-byte library entries: the old
  request for `Stencil::rodata` ranges is obsolete.

## Tests

`src/patch/tests.rs` builds small functions with `FunctionBuilder` and compiles them
against a mock library written with `table::format::Writer`. Each mock stencil is a header
naming its shape, one field per hole (operands, immediates, offset, results, target, trap,
constant) and a `jmp Next` tail. A mock glue writes one marker byte per piece and relays
as `[marker, size, from, to]`. A decoder reads the code back into pieces, so each test states
the code. They cover straight-line accumulator chains with tails dropped, immediate folding
and wide `iconst`s, slot reuse after a last use, global slots, `BrIcmp` (complemented, with
an immediate, and not fused when not adjacent), `SelectIcmp` both ways round, a block-argument
swap through the scratch slot with the back-edge count, far relays on a 140-parameter
function, trap stubs shared per code, dropped jumps, constants placed once and aligned, a
dead result to scratch, `stack_addr`'s offset at the slot's alignment, line rows, errors
naming the opcode, a library without accumulator shapes, a warm compile that allocates
nothing, and determinism. `enc.rs` tests every form at both bounds and one past them.

## State of the art, and where the patcher is faster or slower

- **Xu and Kjolstad, "Copy-and-Patch Compilation" (OOPSLA 2021).** Stencils from LLVM,
  holes as relocations, continuation-passing with `ghccc` tail calls and pass-through
  registers; the paper reports its WebAssembly compiler compiling several times faster than
  V8's Liftoff with faster code. The patcher is the same idea over Cranelift IR. It is
  faster per instruction where the paper's compiler walks an AST and picks among stencil
  variants at run time: selection here is a 4-byte key into a perfect index, and holes are
  fixed-size rows patched by one `match` on a `u8` form. It is slower where the paper's
  stencils keep values in pass-through registers across more than one step: blaze has one
  integer and one float accumulator, and everything else is a frame slot.
- **CPython 3.13/3.14's JIT.** Stencils from clang at build time, one generated C function
  per stencil that copies and patches it, values on the frame's stack. Per stencil, CPython's
  generated patch functions are straight-line code and can beat a table walk over holes.
  CPython compiles traces of micro-ops with no allocation of slots and no edges. The patcher
  does more per instruction: slot reuse, fusions, parallel moves and accumulator chains.
  That costs a few passes over the IR, and pays for itself in code that loads and stores less.
- **TPDE (2025).** A single-pass backend from LLVM IR (or its own IR adaptors) with real
  register allocation, after an analysis pass for loops and liveness; its paper reports
  compiling many times faster than LLVM's `-O0` with code of similar quality. TPDE encodes
  every instruction itself. The patcher copies precompiled bytes instead and does no register
  allocation, so it has less to do per instruction. Its code is slower than TPDE's, because
  every value not in an accumulator is a memory operand.
- **Cranelift's `fastalloc`.** A single-pass reverse register allocator that replaces
  regalloc2's `ion` at `opt_level=none`. It still runs after legalization, ISLE lowering to
  VCode and before emission, and those passes are what the patcher skips entirely: no VCode,
  no instruction encoding, one buffer reservation. The patcher is therefore one to two
  orders faster than Cranelift with `fastalloc`. Its code is slower in loops, where
  `fastalloc` keeps values in registers and the patcher loads and stores slots; tier-up
  (`stencil.md`) recompiles hot functions with Cranelift.

Where the patcher is slower than it could be, and why it is so for now:

- Six passes over the instructions (scan, select, slots, edges, size, emit) instead of one
  or two. Each pass is simple and touches flat columns, and splitting them keeps the size
  pass exact, so the buffer never grows. A fused scan+select pass is the first thing to
  try if the measured cost per instruction is over 60 ns.
- Edges with moves or back-edge counts get code of their own and one more jump.
- Far slots in large frames cost a relay (a load and a store) per far operand.
- Moves on edges are `Copy` stencils between slots, not register moves.
