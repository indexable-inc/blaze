# Requests from fastcompile-stencil-gen

## fastcompile-stencil-gen 1: fastcompile-stencil: `stencil.md` is behind the generator (`docs/v2/plan/lanes/fastcompile/stencil.md`)

What, three places in the parent's doc:

- Line 175 says the generator compiles with `-stack-size-section` and refuses a stack size
  other than 0. LLVM's C API has no setter for `TargetOptions::EmitStackSizeSection` (only
  the C++ `TargetMachine` does), and blaze-llvm-sys binds only the C API. The generator
  instead refuses, from the disassembly, every instruction that names the stack pointer, the
  frame pointer, x18, x29 or x30, and every push, pop, call and return (`bl`, `blr`, `ret` on
  AArch64). That is stricter than a stack size of 0: it also refuses a stencil that keeps a
  zero-size frame but touches the stack or a reserved register. The line could say that.
- Line 211 gives a 12-byte stencil row. Since request `fastcompile-stencil-patch 1` the
  library is format 3: a 16-byte row whose last word is `first fault u24 | faults u8`, a
  `u16` fault column after the holes, and an 84-byte header (eight counts, then the id).
  `src/table/format.rs`'s module docs have the layout.
- Line 303 says `LLVMParseIRInContext`; the generator uses `LLVMParseIRInContext2`, which
  does not take ownership of the buffer.

Why: the doc is `current`-adjacent design the siblings read; the generator owns none of it.

Status: done by fastcompile-stencil in stencil.md (all three places).

> forwarded to fastcompile (from requests, pass 16; told by tmux).
