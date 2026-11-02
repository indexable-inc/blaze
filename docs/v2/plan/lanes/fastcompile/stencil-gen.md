---
status: plan
summary: blaze v2 lane fastcompile, child fastcompile-stencil-gen of stencil.md: the generator in crates/blaze-stencil/gen (run by build.rs) that has LLVM make every stencil at blaze's build time, how each hole is made in the IR per instruction set, the checks that refuse a stencil, the probe of the pinned registers, the library's reader and PTHash index, and how it compares with CPython's JIT and Xu and Kjolstad's generator.
date: 2026-10-29
---
# fastcompile: the stencil generator (`blaze-stencil`'s `gen/`)

Part of [stencil.md](stencil.md) ("Shapes", "Holes", "Stencils' calling convention and pinned
registers", "The library", "Why a build script", "Semantics, op by op"). The code:
`crates/blaze-stencil/build.rs` and `gen/` (`mod.rs` the driver, `ir.rs` the IR of a stencil,
`ops.rs` each op's body, `llvm.rs` LLVM's C API, `elf.rs` the object and the checks);
`src/shape.rs` the set, `src/table/format.rs` the library's bytes, `src/table.rs` the
libraries built in.

## What a build does

For each variant (`x86_64`, `aarch64`, `aarch64_lse`), on its own thread with its own LLVM
context, on any host:

1. **IR.** `shape::each_on(variant.isa())` in its one order, each shape one `preserve_none`
   function `(ptr frame, i64 acc, <2 x i64> facc)` in its own section, ending in a `musttail`
   call of `hole.next`. One module of about 7,000 functions, plus the probe.
2. **Compile.** `LLVMParseIRInContext2`, verify, then one target machine at -O2: x86-64 cpu
   `x86-64`, PIC, small code model, `+cx16` only on the `i128` `atomic_cas` stencils;
   AArch64 cpu `generic`, static, tiny code model, `+reserve-x18`, atomics inline, `+lse` for
   `aarch64_lse`. Every function `nounwind` with `"frame-pointer"="none"` and no unwind
   table. One ELF object in memory (`*-unknown-linux-gnu`: the bytes are the same for every
   OS).
3. **Read.** Each stencil's section is its code. Each relocation is a hole: its ELF type is
   its `enc::Form`, its symbol its `HoleKind` (or a constant of LLVM's pool, moved into the
   library's rodata as a `Rodata` hole), its addend ELF's. `tail` is the trailing
   `jmp rel32`/`b imm26` whose field is `Next`.
4. **Write.** `format::Writer` in the set's order; the PTHash index; the id is BLAKE3 of the
   bytes. `OUT_DIR/<variant>.stencils`, which `table.rs` includes.

The output is a function of the sources and the pinned LLVM only: the module text is built
in a fixed order (intrinsic declarations in a `BTreeMap`), LLVM's codegen is deterministic,
and holes are sorted. Two runs give equal bytes.

## How a hole is made

- **x86-64.** Number holes (`Slot*`, `Result*`, `Imm*`, `Offset`) are external hidden
  globals with `!absolute_symbol` ranges (`[0, 2^31)` for slots, `[-2^31, 2^31)` for the
  rest). LLVM treats such a symbol as a constant it does not know, folds it into the
  displacement or immediate that uses it and leaves an `R_X86_64_32S`/`32`/`8`: the field is
  the instruction's own. Two exceptions are inline assembly: a 64-bit `iconst` (`movabsq`,
  `R_X86_64_64`) and a 16-bit immediate (`movl`, as an `R_X86_64_16` has no form). Places
  (`Next`, `Const`, constants) are RIP-relative (`R_X86_64_PC32`/`PLT32`).
- **AArch64.** LLVM would address any symbol PC-relatively, so slot accesses are inline
  assembly with explicit operators, each one `ldr`/`str [x20, #:lo12:hole]`
  (`LDST64_ABS_LO12_NC`, or `LDST128` for 16-byte values), and immediates are
  `movz`/`movk` of `:abs_g0:` to `:abs_g3:`. The tiny code model makes constants `adr` or
  `ldr` literal, so no `adrp` page delta depends on where the code lands.
- **Both.** A conditional exit (a trap, a branch's target) is inline assembly branching to
  the hole's symbol, so the stencil stays one block whose last instruction is the jump to
  `Next`.

## What refuses a stencil

Each is a panic naming the stencil (`iadd.i64.0.si.a (X86_64)`), which fails blaze's build:

- an instruction naming the stack, frame or link register or `x18` (`rsp`, `rbp`; `sp`,
  `x29`, `x30`, `x18` and their aliases), or pushing, popping, calling or returning. LLVM's
  disassembler reads every instruction. This is also the stack-size check: a stencil with a
  frame must touch `rsp`/`sp`. `-stack-size-section` is not used because LLVM's C API has no
  setter for `TargetOptions::EmitStackSizeSection`, and the disassembly check is stricter (it
  also refuses red-zone use, which a stack size of 0 allows);
- a relocation of a type no `Form` is, against a symbol that is neither a hole nor a constant
  of the pool, outside its stencil, with an addend past 32 bits, of a place kind in an
  absolute field or the reverse, or of a kind the shape has no use for (`Slot1` where
  operand 1 is an immediate, `Target` in a non-branch), and a missing `Result` or `Next`
  hole;
- read-only data other than `.rodata.cst{4,8,16}`, or a pool with relocations;
- a section that holds more than its function.

**The probe** stores `frame`, `acc` and `facc` to `Result0`, `Result1` and `Slot0`. The
generator finds the instruction of each hole and requires `%r12`, `%r13`, `%xmm0` stored at
`(%r12)` on x86-64 and `x20`, `x21`, `q0` at `[x20` on AArch64: `shape::pinned`, which the
crate's tests hold equal to `arch::x64::X64::FRAME` and `arch::aarch64::Aarch64::FRAME`.

## The reader

`Reader::new` checks everything once: header, every column's place from the counts, every
hole's form, kind, place and rodata entry, every slot's key and stencil, and that the index
sends every key to its own slot. After that `find` is three multiplies, one pilot and one
slot (a compare), and `stencil` one 16-byte row and three slices: no allocation, no checks
beyond slicing.

**Faults.** For a load, a store or an atomic, the row also names where in the stencil each
access of the op's memory starts (`Stencil::faults`, a `u16` column), so the patcher can
record the instruction's `MemFlags` trap code there, as Cranelift does at each trapping
access (request `fastcompile-stencil-patch 1`). The generator finds them in the disassembly:
the memory operands whose base is not the frame register and which are not PC-relative. A
memory op with none fails the build. The index is PTHash: about 5 n / log2 n buckets, load factor about 0.95,
16-bit pilots, buckets placed largest first, retrying with another seed and then more buckets
when a bucket finds no pilot.

## State of the art

- **CPython 3.13/3.14's JIT (`Tools/jit`).** It runs clang on C templates per micro-op, reads
  the object with LLVM's tools (`llvm-readobj --elf-output-style=JSON`), and writes C:
  each stencil's bytes and one generated C function per stencil that patches its holes, with
  a `switch` on the opcode to find it. It also uses `preserve_none` with `musttail` (3.14).
  The generator here matches its approach and beats it in four places. It writes the IR
  itself, so `musttail`, the convention, `!absolute_symbol` ranges and exact AArch64
  addressing are chosen, not hoped for from C. It compiles one module per variant in one
  process through the C API, with no clang, no `llvm-readobj` and no JSON. It checks every
  stencil (no stack, no reserved register, every relocation a known form, the pinned
  registers by a probe) instead of trusting the compiler. And it writes data, not code: holes
  are 8-byte rows patched by a `match` on a `u8` form, and the index is a perfect hash.
- **Xu and Kjolstad (OOPSLA 2021).** Their MetaVar generator builds stencils from C++
  templates through clang, with `ghccc` and pass-through registers, and enumerates
  configurations by template metaprogramming. Here the configuration space is an explicit,
  pruned set (`shape::each_on`, about 7,000 per variant) shared by the generator, the patcher
  and the tests, and the semantics are Cranelift's, not C's, op by op.
