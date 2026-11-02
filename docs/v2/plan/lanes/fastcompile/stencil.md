---
status: plan
summary: blaze v2 lane fastcompile, idea 4 (copy-and-patch codegen for dev builds): crates/blaze-stencil, its input (Cranelift IR after the IR builder's ABI lowering, so both dev backends share one lowering of MIR), the shapes and the hole kinds, the stencils' calling convention and pinned registers, the value slots and the accumulator, the library format and its build-time perfect index, the glue per architecture written as Cranelift writes it, the patcher's one pass and one buffer, unwind and line tables, tier-up from per-function counter cells, why the stencils are made by a build script through llvm-sys and how that keeps no-rustc and blaze-builds-blaze, the tests, the children and the hooks requested of codegen, link and the coordinator.
date: 2026-10-29
---
# fastcompile: copy-and-patch codegen (`crates/blaze-stencil`)

Idea 4 of [compile-time.md](../../design/compile-time.md), and tier 0 of idea 13. When blaze is
built, LLVM compiles one *stencil* per operation, operand shape and type: a few machine
instructions with *holes* where a frame offset, a constant or a jump target goes. Compiling a
function is then copying the stencils of its instructions into one buffer and patching their
holes. Hot functions are recompiled with Cranelift or LLVM and patched into their slots.

Owned here: `crates/blaze-stencil/` and this file. Hooks in other lanes are requests in
`v2/requests/fastcompile.md` (`fastcompile-stencil 1` and on).

## The input: Cranelift IR after ABI lowering

The backend compiles the `ir::Function` that `hir-ty-codegen`'s IR builder makes for the
Cranelift backend. It is the IR `v2/backend.rs`'s `compiled` hands to `crate::emit`, after
`float_pair::lower` has run. So:

- **One lowering of MIR.** Calls, the Rust ABI, drops, panics, unwinding (`try_call`),
  overflow checks, intrinsics, `f16`/`f128` (already calls, `f16_f128.rs`), `asm!` (already
  assembled), statics, thread-locals and `#[track_caller]` locations are lowered once, by the
  IR builder's rules. The two dev backends cannot disagree on what a program means. They can
  only disagree on how an IR operation runs, and that is what the tests check, one operation at
  a time.
- **One ABI.** Signatures are Cranelift's (`SystemV`, `WindowsFastcall`, `AppleAarch64`, the
  three conventions the IR builder uses). The glue passes arguments and returns values exactly
  as Cranelift's `isa/{x64,aarch64}/abi.rs` assign them, including `enable_llvm_abi_extensions`
  for `i128`, `StructArgument`, `StructReturn` and multiple returns. Stencil code and Cranelift
  code call each other with no adapter, so a tier-up is one slot store.
- **The same relocations.** Calls, symbol addresses and thread-local accesses are written as
  Cranelift writes them for the same flags (`is_pic`, `tls_model`, `colocated`). They carry
  Cranelift's `binemit::Reloc` kinds, so codegen's `convert.rs` maps them to `RelocKind`s
  exactly as it maps Cranelift's.

Not MIR directly: a second lowering of MIR would be a second copy of the IR builder's 20,000
lines (calls, drops, unwinding, intrinsics), with two places for every rule to drift apart.
Building Cranelift IR costs about a fifth of Cranelift's compile. The remaining four fifths
(legalize, egraph, lowering, regalloc, emit) are what copy-and-patch replaces.

## What a compile is

`blaze_stencil::compile(func, target) -> Result<Code, Error>` makes one pass over the IR and
writes one buffer:

1. **Scan** (`patch/scan.rs`). Use counts, which values are `iconst`s (folded as immediates
   where a shape takes one), and the last use of each value local to one block. Each is a
   column indexed by `ir::Value`, on the compile's frame. Selection
   (`patch/select.rs`) then picks each instruction's shape, taking a fusion only when the
   library has the fused shape.
2. **Slots** (`patch/slots.rs`). Which result stays in the accumulator is decided here, after
   selection, so it can ask the library for the accumulator shape. Every value that is not
   folded or kept in the accumulator gets a slot in the frame: 8
   bytes for scalars up to 64 bits, 16 for `i128`, `f128` and vectors. A value used only in
   its own block frees its slot after its last use, and the next definition reuses it: a
   linear scan in layout order, with no liveness analysis across blocks. Values that cross
   blocks keep their slot for the whole function. Stack slots (`ir::StackSlot`) are laid out
   after the value area, at their alignment.
3. **Frame** (`A::frame`). The architecture lays out saved registers, the outgoing argument
   area (the largest of the calls' stack arguments) and the probe. It realigns the frame
   when a stack slot asks for more than 16 bytes.
4. **Size.** It adds up each instruction's stencil size (from the library) and each glue
   instruction's bound (`A::bound`), then reserves the buffer once. Nothing grows per stencil.
5. **Emit**, block by block in layout order. Each instruction is either a stencil (select its
   shape, look up the stencil, copy its bytes, patch its holes) or glue (the architecture
   writes it). A stencil whose last instruction is the jump to the next stencil is copied
   without that jump (`tail` bytes, recorded by the generator) when the next code follows
   directly. Straight-line code is then the stencils' bodies back to back.
6. **Edges.** Block arguments are parallel moves between slots. Copy stencils write them,
   with a cycle broken through a scratch slot. An edge gets its own label and code after
   the block when it has moves, is a back edge (for its count) or is a landing pad. Any other
   edge jumps straight to its block, and a jump to the next block is dropped.
   [stencil-patch.md](stencil-patch.md) has the patcher's passes in their exact order.
7. **Finish.** It writes trap stubs (one per trap code used), then the function's constants
   (`vconst`, shuffle masks, 16-byte aligned, reached PC-relatively). Label fixups are
   resolved and the unwind rows closed, and the result is returned as `Code`: the bytes and
   their alignment, the relocations, the unwind information, the call sites and their
   handlers, the line and trap tables, where each stack slot is (`slots`), and `frame`, the
   bytes from the frame pointer down to the stack pointer in the body (Cranelift's
   `frame_to_fp_offset`, `Glue::frame_size`). A stencil frame is fixed, so `frame` is one
   number per function: the stack pointer a landing pad is resumed with is `fp - frame`.

The compile allocates nothing from the global allocator. It runs as one blaze-mem task
(`blaze_mem::frame::task(Tag::Codegen, ..)`, [memory.md](../../design/memory.md)): every column,
the buffer and the side tables are vectors on the task's frame, made empty at the start and
gone when the frame is reset at the end, and the thread keeps the frame warm for the next
compile. The result is handed out borrowed from the frame (`CodeRef`) to a sink:
`blaze_stencil::compile_to(func, target, sink)`. `compile_into(func, target, &mut code)` copies
it into a reused `Code` (`Code::copy_from`), so a warm compile allocates nothing at all, and
`compile` copies it into a fresh `Code`.

## Shapes

A shape (`shape::Shape`, 4 bytes packed in `Shape::key`) names one stencil:

- `op`: the operation (`shape::Op`, a `u8` enum): Cranelift's opcodes that are pure,
  memory or branch operations, with the `_imm` forms folded into their base op.
- `ty`: the controlling type (`shape::Ty`): `i8` to `i128`, `f16` to `f128`, and the 128-bit
  vectors `i8x16`, `i16x8`, `i32x4`, `i64x2`, `f32x4`, `f64x2`. These are the types the IR
  builder makes. `f16` and `f128` have only moves, loads, stores, `select` and `bitcast`.
  Their arithmetic is calls already.
- `aux`: what else changes the code. That is the condition code of `icmp`/`fcmp`/`brif`
  fusions, the `AtomicRmwOp`, the lane of `extractlane`/`insertlane`, the source type of an
  extension, reduction or conversion, or the access and offset form of a confined access.
- `srcs`: where each operand comes from: a slot, the accumulator, or an immediate (32 bits,
  sign-extended to the type).
- `dst`: where the result goes: a slot, the accumulator, or both.

Fusions are shapes of their own. `icmp`/`icmp_imm` followed by the `brif` that is its only use
is one `BrIcmp` stencil, which compares and branches with no `setcc` and no slot. `select` of
an `icmp` is one `SelectIcmp`. These two cover the loop conditions and the `min`/`max` idioms
of MIR. A load or store (any type and extension, and `atomic_load`/`atomic_store`) at offset 0
of the compiled macro tier's confined address,
`iadd vmctx, uextend.i64 (ireduce.i32 (iadd p, iconst off))`, each inner value used once, is
one `LoadConfined` or `StoreConfined` stencil
([macros-tier.md](macros-tier.md), "Cost per access"): x86-64
`mov eax, [r12+p]; lea eax, [rax+off]; mov r, [rbase+rax]`, AArch64
`ldr w, [x20, #p]; add w, w, #off; ldr x, [xb, w, uxtw]`. Its `aux` names the plain access
(`Shape::confined`) and how `off`, wrapped to 32 bits, is added: not at all when it is 0,
AArch64's `add #:lo12:` for 1 to 4095, or a 32-bit `Offset` hole. Its fault is the access's,
with the access's trap code. Without the pattern, or without the fused shape in the library,
each instruction is its own stencil, so code without it is unchanged.

**The accumulator** keeps a result in a register when its only use is the next instruction,
or in a register and its slot when it has other uses too. This is top-of-stack caching (Xu and
Kjolstad's "pass-through" registers, CPython 3.14's cached stack top). An expression chain
`a + b * c - d` runs with one load per leaf and no store between steps. There is one integer
accumulator and one float/vector accumulator, and at most one operand per shape comes from
one. Glue never reads an accumulator: a value whose next use is glue goes to its slot. A
block starts with both accumulators dead.

The set is enumerated by `shape::each`, one deterministic order shared by the generator, the
tests and the index. `Op::info` gives each op its arity, results, types and which operand
positions take an immediate or an accumulator. Selection (`patch/select.rs`) maps
`ir::InstructionData` to a shape. An instruction with no shape and no glue is an error
naming the opcode and type, never a compile by Cranelift. `tests/coverage.rs` holds the set
to every (opcode, type) Cranelift's x64 and AArch64 lowerings accept, so the error is
unreachable for any IR Cranelift compiles.

Count: 7,000 to 8,000 stencils per architecture variant (7,408 on x86-64 and 7,782 on AArch64,
of which the confined accesses are 508 and 762), 200 to 300 KB of code and tables per
variant.

## Holes

A hole is a relocation LLVM made in the stencil, recorded by the generator as
`(at, form, kind, addend)` (`table::Hole`, 8 bytes).

| kind | value patched in |
|---|---|
| `Slot(n)` | frame offset of operand `n`'s slot |
| `Result(k)` | frame offset of result `k`'s slot |
| `Imm(n)` | operand `n`'s immediate |
| `Offset` | the memory access's offset immediate |
| `Next` | the next instruction's code (jump to the continuation) |
| `Target` | the taken branch's edge label |
| `Trap` | the trap stub of the instruction's trap code |
| `Const(n)` | PC-relative address of the instruction's 16-byte constant in the function's constant area |
| `Rodata` | PC-relative address of the stencil's own read-only bytes (LLVM's constant pool), placed once per function |

`form` is how the field is encoded (`enc::Form`):

- x86-64: `Abs8`, `Abs32`, `Abs32S` and `Abs64` (immediates and displacements, from
  `!absolute_symbol` ranges), and `Pc32` (`jmp`/`jcc`/RIP-relative).
- AArch64: `MovwG0` to `MovwG3` (`movz`/`movk`); `Ldst{8,16,32,64,128}Lo12` and `AddLo12`
  used on an absolute value (the imm12 field, scaled by the access size, range-checked);
  `Jump26`, `CondBr19`, `TestBr14`; `AdrLo21` and `LdrLit19` for constants (the tiny code
  model's PC-relative forms, ±1 MB, so a function's own constants need no linker relocation
  and no `adrp`, whose page delta would depend on where the linker puts the code).

`enc::write` patches one field and checks its range. A value out of range is an error naming
the instruction and the form, never a truncated field.

## Stencils' calling convention and pinned registers

Each stencil is an LLVM function in the `preserve_none` convention (`CallingConv::PreserveNone`,
21, LLVM 19 and later, the one CPython 3.14's JIT uses). It ends in a `musttail` call of the
continuation hole, which LLVM emits as a single `jmp`/`b`. The stencil signature is
`(ptr frame, i64 acc, <2 x i64> facc) -> void`:

| | x86-64 | AArch64 |
|---|---|---|
| `frame` (base of the value area) | `r12` | `x20` |
| `acc` | `r13` | `x21` |
| `facc` | `xmm0` | `v0` |

`preserve_none` passes its first integer arguments in `r12, r13, ...` on x86-64 and
`x20, x21, ...` on AArch64. The generator does not trust that list. It compiles a probe
stencil and reads the registers from its bytes, and the build fails if they are not the ones
`arch::x64::X64::FRAME` and `arch::aarch64::Aarch64::FRAME` name. `r12` and `x20` are
callee-saved in SysV, Win64 and AAPCS64, so `frame` survives every call the glue makes with no
reload.

Stencils are **leaf and stackless**. LLVM's C API has no setter for the stack-size section,
so the generator checks each stencil's disassembly instead, which is stricter: it rejects
every instruction that names the stack pointer, the frame pointer, `x18`, `x29` or `x30`,
and every push, pop, call and return. It also rejects any relocation other than a hole. A stencil never calls, never spills and never moves `rsp`/`sp`, so stack alignment, CFI
and unwinding never depend on stencil code. The operations Cranelift itself lowers to a
libcall on the baseline CPU are glue calls of the same libcall: `ceil`, `floor`, `trunc` and
`nearest` without SSE4.1, `fma` without FMA, `X86Pshufb` without SSSE3. They run the same
code Cranelift's does.

**Registers stencils write.** `preserve_none` leaves `rbp`/`rsp` (x86-64) and
`x29`/`x30`/`sp` (AArch64) to the callee. Every other register may be written. So the
prologue saves every register the target's C ABI calls callee-saved: `rbx` and `r12` to
`r15`, plus `rdi`, `rsi` and `xmm6` to `xmm15` on Win64, and `x19` to `x28` and `d8` to `d15`
on AArch64. AArch64 stencils are built with `+reserve-x18` (the platform register on Apple
and Windows), and the generator checks that none writes it.

**Slots in memory.** A slot holds its value in its low bytes, and the rest is unspecified. A
stencil reads and writes at least its type's width and never past its slot. On AArch64 every
scalar slot access is an 8-byte `ldr`/`str [x20, #imm12*8]` (one instruction up to 32 KB of
value area) and every 16-byte one a `ldr q`/`str q` (64 KB). Operands farther than that are
relayed by the glue through two near scratch slots (`A::relay`). On x86-64 every access is a
`disp32`, so nothing is far.

## The library

One library per architecture variant, chosen by `Target::cpu`, matching the CPU baselines
`hir-ty-codegen/target.rs` gives Cranelift:

- `x86_64`: x86-64 baseline. `+cx16` is set only on the `i128` `atomic_cas` stencils, as
  Cranelift enables `has_cmpxchg16b`.
- `aarch64`: AArch64 generic.
- `aarch64_lse`: AArch64 generic plus `+lse`, for `apple-m1` (Cranelift's `has_lse`).

Format (`table/format.rs`, little-endian, read in place):

```
Header   84 bytes: magic "BZSTENC1", FORMAT (3), shape::VERSION, variant, counts, seed, id
Stencil  [stencils] { code: u32, len: u16, tail: u8, holes: u8, first_hole: u32,
                      first_fault: u24 | faults: u8 }                              16 bytes
Hole     [holes]    { at: u16, form: u8, kind: u8, addend: i32 }                   8 bytes
Fault    [faults]   u16: where an access of the op's memory starts
Index    pilots [buckets] u16, slots [n] (key: u32, stencil: u32)                  PTHash
Code     bytes, padded to 16
Rodata   [entries] 16 bytes each, named by `Rodata` holes
```

Every column's offset follows from the counts. A load's, store's or atomic's faults are where
it can trap on memory; the patcher records each with the instruction's trap code in
`Code::traps`, as Cranelift records one at each trapping access. `table/format.rs`'s module
docs are the exact layout.

The index is a perfect hash built by the generator (PTHash: one pilot per bucket, a
multiply-shift hash), from `Shape::key` to the stencil. A lookup is two multiplies, one load
of a pilot and one compare, with no allocation and no load-time construction. The library is
`include_bytes!`'d into blaze. `table::Library::get` checks its bounds and header once,
behind a `OnceLock`, and reads it in place after that. `id` is the BLAKE3 hash of the
library's bytes. It goes into the codegen `Settings` of every stencil unit (request 1), so a
changed stencil set is a changed key.

## The glue per architecture

`arch::Glue` (one implementation per architecture, `arch/x64.rs` and `arch/aarch64.rs` with
their directories) writes what no stencil can be, exactly as Cranelift writes it for the same
IR and flags:

- **Prologue and epilogue**: `push rbp; mov rbp, rsp` (`stp x29, x30, [sp, #-16]!; mov x29,
  sp`), then the saved registers, then the probe and the frame adjustment, then `frame`.
  Cranelift's inline `probestack` loop for frames larger than a page (`probe_log2`), and a
  realignment when a slot asks for it. The epilogue reverses it. Every function keeps a frame
  pointer, so Cranelift's `preserve_frame_pointers` behavior and `get_frame_pointer` hold.
- **Parameters and returns**: incoming arguments stored into their block-parameter slots,
  returned values loaded from slots, in the registers and stack places Cranelift's ABI code
  assigns (`arch/x64/abi.rs`, `arch/aarch64/abi.rs`, ported from cranelift-codegen 0.136.2's
  `isa/*/abi.rs` and `machinst/abi.rs`).
- **Calls**: `call`, `call_indirect`, `try_call`, `try_call_indirect`, `return_call`,
  `return_call_indirect`. It loads arguments from slots, calls with Cranelift's relocation,
  records the call site with its exception handlers, and stores results to slots (for
  `try_call`, the pseudo slots of `TryCallRet`). At a landing pad it stores the exception
  payload (`rax`/`rdx`, `x0`/`x1`) into the pseudo slots of `TryCallExn`.
- **Addresses**: `func_addr`, `symbol_value` and `tls_value` (Cranelift 0.136 has no
  `global_value`), written as Cranelift writes them for `is_pic` and `tls_model` (GOT loads for
  what is not colocated, `__tls_get_addr`/TLS descriptors/`tlv` thunks/`_tls_index`).
- **Other**: `br_table` (bounds check, then a table of 32-bit offsets in the code),
  `get_frame_pointer`, `get_stack_pointer`, `get_return_address`, `trap`, `debugtrap`, the
  libcall operations, the far-slot relays, and the tier counters.

**Unwind.** The glue records frame rows as it writes the prologue and epilogue:
`Rule::Cfa`, `Offset`, `RememberState` and `RestoreState`, the same instructions
`hir-ty-codegen/unwind.rs`'s `FrameRule` has. On Windows it writes the `UNWIND_INFO` codes
of the same prologue (frame register `rbp`). The CFA is said from the frame pointer, so it
holds at every instruction of the body. Call sites with their handlers give codegen what it
needs to write the LSDA (`lsda::write`).

**Lines.** The patcher records `(offset, func.srcloc(inst))` where the source location
changes. Codegen turns them into `LineRow`s with `LineRow::of_ranges`, as it does for
Cranelift's `get_srclocs_sorted`. Named slots are given by their offset from the frame
pointer, for `SlotPlaces`.

## Tier-up

Tier 0 code counts (`tier.rs`). The prologue adds 1 to the function's *calls* cell and each
back edge adds 1 to its *back edges* cell. A back edge is a branch to a block at or before the
branch in layout order. The two cells (16 bytes) are reached through one relocation,
`code::Name::Counter`, which codegen gives the linker as `Target::Counter(InstanceKey)`
(request 1). The linker places every function's cells in one writable region of the image.
In-process (idea 6) that region is in the dual-mapped memory, so the daemon reads the counts
in place with relaxed loads. There is no sampler, no file and no channel round trip.

The add is a plain (not locked) read-modify-write: `add qword [rip+cell], 1` on x86-64, and
`adrp`/`ldr`/`add`/`str` through `x16`/`x17` on AArch64. Lost counts under contention are
fine for a heuristic, and an aligned 8-byte store never tears. HotSpot's and V8's invocation
counters are racy the same way. Release and `blaze build` outputs never contain stencil code,
so they never count.

Cranelift code of a tiered run counts the same way: `tier::instrument` adds the same two
adds to its IR before Cranelift compiles it, against a user name in `tier::COUNTER_NAMESPACE`
that codegen resolves to the unit's `Target::Counter` (request 2), so one mechanism measures
both promotions. Whether a unit counts is in its settings (`Spec::counted`), so counting and
plain Cranelift code are different instances. LLVM code, the last tier, does not count.

`tier::Tiering` is a deterministic state machine over one 16-byte entry per function (tier,
flags, the weighted count at the last snapshot, a decayed rate). Fed snapshots of the cells
(`tier::Counters`, `tier::Driver`), it answers which functions to recompile and at which
tier, at most one in flight per function, tiers only rising, and a failed tier never asked
for again in the run. A function moves to Cranelift past `Policy::warm` (4,096 calls plus
back edges × `back_edge_weight`) and to LLVM past `Policy::hot` (65,536, counted in
Cranelift's code); `tier.rs` argues both from compile costs by the ski-rental rule. The
daemon's `tier::Recompile` compiles the unit at `spec.at(backend)` and publishes the new code
through blaze-lazy's placer and blaze-link's slot store, driven by `tier::Runner`. The old
code is retired behind blaze-inproc's fence. Hot counts are kept by function identity in `blaze-history`, so the
next run starts hot functions at their last tier (idea 15).

## Why a build script

The stencils are made when blaze is built, by `build.rs` running the generator in `gen/`. The
generator drives LLVM through `vendor/llvm-sys` (a build dependency): it writes each stencil's
LLVM IR as text, parses it with `LLVMParseIRInContext2`, compiles one module per variant with
`LLVMTargetMachineEmitToMemoryBuffer`, reads the object with `object`, and writes the library
to `OUT_DIR`. The alternatives:

- **A generator binary with its output checked in, verified by a test.** That makes two
  sources of truth, and an LLVM bump makes a diff of opaque bytes that has to be
  regenerated by hand. A blaze built without LLVM could keep its stencils, but such a blaze
  has no dev tier anyway. The checked-in bytes are machine code nobody reviews.
- **clang over C templates (CPython's way).** That adds a C compiler and a second frontend
  to blaze's build. Writing the IR directly also controls exactly what each stencil is:
  `musttail`, `preserve_none`, `!absolute_symbol` ranges, inline assembly where an
  addressing mode has to be exact.

So the build script, which keeps both rules:

- **no-rustc**: nothing runs rustc. The build script is Rust compiled by blaze's own front
  end and Cranelift like any build script. LLVM is a library it links, as `hir-ty-codegen`'s
  `llvm` feature already links it.
- **blaze-builds-blaze**: blaze builds `blaze-stencil` like any crate with a build script:
  sandboxed (sandbox-everything), writing only `OUT_DIR`, with no network and reading only
  its sources and the pinned LLVM prefix (`LLVM_SYS_231_PREFIX`, which llvm-sys's own build
  script reads). Its output is cached by blaze's build-script key (sources, LLVM pin,
  target), so the stencils are remade only when the generator, the shape set or LLVM
  changes. That takes a few seconds for ~21,000 stencils across the three variants.
- **no-fallback**: a build of blaze with no LLVM prefix fails in this build script with an
  error naming `LLVM_SYS_231_PREFIX`. It does not make a blaze with no tier 0. The pinned
  LLVM is in every blaze build environment already (the flake, CI).

The shared definitions are plain-std files that `build.rs` includes with `#[path]`:
`src/shape.rs`, `src/enc.rs` and `src/table/format.rs`. They use nothing of the crate but
each other (`crate::shape`, `crate::enc`), so the generator and the patcher read one
definition of the shapes, the hole forms and the format.

## Semantics, op by op

Each stencil's LLVM IR implements the Cranelift operation's semantics, not LLVM's similar
instruction. For example:

- `fmin`/`fmax` are `llvm.minimum`/`llvm.maximum` (NaN in, NaN out; `-0 < +0`).
- `udiv`, `sdiv`, `urem` and `srem` check for zero and for `MIN / -1` and go to the `Trap`
  hole (`int_divz`, `int_ovf`), as Cranelift's do.
- Shifts mask the amount by the type's width.
- `fcvt_to_*` without `_sat` trap on NaN and out-of-range values, and with `_sat` saturate.
- `nearest` rounds half to even.
- Atomics are sequentially consistent, as Cranelift's are, with LSE in `aarch64_lse` and
  exclusive-pair loops otherwise.
- Loads and stores are little-endian. A `MemFlags` that asks for big-endian is an error.
- NaN payloads come from the same hardware instructions Cranelift uses (`addss`, `fadd`),
  and none are canonicalized, as Cranelift's are not.

## Tests (`src/tests.rs`, `src/tests/`)

- **Per stencil, against a reference interpreter.** For every shape, random operands
  (edges first: 0, ±1, MIN, MAX, NaNs with payloads, ±0, subnormals) are run through the
  stencil, placed by itself in a harness frame, and through `cranelift-interpreter` on the
  one-instruction function. Results and traps must be equal bit for bit.
- **Per function, against Cranelift.** Generated IR functions (random straight-line code,
  loops, block arguments with swaps, calls across both backends, `try_call` with landing
  pads, `br_table`, every signature shape of the three conventions) are compiled by both
  backends, loaded into executable memory and run on the same inputs. The results, the
  memory written and the traps must be equal. Stencil code calls Cranelift code and the
  reverse.
- **The corpus.** Whole programs run in proof's harness: request 4 asks for a backend axis in
  `harness/v2/corpus.py`, so every corpus program runs with the dev tier forced to the stencil
  backend and to Cranelift, and must give the same output, exit status and panics. The
  coordinator runs it. In the crate, `src/tests/clif/` holds a corpus of functions in Cranelift's
  text format, parsed with `cranelift-reader`, shaped like what the IR builder makes
  (drop glue, `try_call` with cleanup pads, overflow-checked arithmetic, `match` as
  `br_table`, iterator loops, `i128` math, SIMD intrinsics). `tests/corpus.rs` runs each one
  through both backends. blaze-stencil cannot run the IR builder itself: `hir-ty-codegen`
  depends on it, and a dev-dependency back would be a cycle.
- **Library.** Every shape `each` lists has a stencil. Every hole's form is one `enc` patches.
  No stencil has a stack, a call or a write of a reserved register. The index finds every
  key, and a missing one is `None`. The library bytes are deterministic (two generations
  give equal bytes).
- **enc**: each form round-trips every in-range value and refuses out-of-range ones, checked
  against `object`'s and LLVM's encodings of the same relocation.
- **unwind**: the rows of each prologue shape, compared with what Cranelift's
  `create_unwind_info` gives for a function with the same frame, and a real unwind through
  stencil code by the system unwinder (`_Unwind_Backtrace`) in the per-function tests.
- **tier**: the state machine over random snapshots: monotone tiers, determinism, no
  promotion below the thresholds, and one recompile per promotion.

## State of the art, and how it is beaten

- **Xu and Kjolstad, "Copy-and-Patch Compilation" (OOPSLA 2021)**: stencils from LLVM with
  holes as relocations, CPS with `ghccc` tail calls, and pass-through registers. Their WebAssembly
  compiler is about 5x faster to compile than Liftoff and its code 4.9 to 6.5x faster than
  Liftoff's. blaze takes the same approach over Cranelift IR, with the accumulator as
  pass-through registers and `preserve_none` in place of `ghccc`.
- **CPython 3.13/3.14's JIT**: stencils from clang at build time, holes as relocations, a
  generated table, `preserve_none` with `musttail`. blaze beats it on compile speed with
  three things CPython doesn't have. A perfect-hash index replaces C switch dispatch. Holes
  are recorded as fixed-size rows patched by a `match` on a `u8` form, where CPython
  generates a C function per stencil. The output buffer is one reservation per function.
- **Compile speed target**: Cranelift at `opt_level=none` takes about 10 to 30 µs for a
  100-instruction function. The patcher's work per instruction is a scan step, a shape, one
  index lookup, one `memcpy` of about 20 bytes and about 2 hole patches: about 30 to 60 ns,
  which makes it 50 to 300x faster than Cranelift's backend and about 10x faster than the
  whole path once the IR builder is counted. `tests/speed.rs` measures it on the corpus's
  IR, and proof's budgets hold it.
- **Code quality**: close to Cranelift's `opt_level=none` for straight-line code (the
  accumulator removes most slot traffic). Loops pay a load and store per value per
  iteration, and tier-up takes those away once they are hot.
- **TPDE (2025)** is a single-pass backend from LLVM IR at about 10 to 20x LLVM -O0's speed,
  with register allocation. It is the next step if tier 0 code quality ever matters more
  than compile time. The slot model keeps the patcher simpler and faster to compile.

## Children

| child | paths |
|---|---|
| `fastcompile-stencil-gen` | `build.rs`, `gen/`, `src/shape.rs`, `src/table.rs`, `src/table/` |
| `fastcompile-stencil-patch` | `src/patch.rs`, `src/patch/`, `src/buf.rs`, `src/code.rs`, `src/enc.rs` |
| `fastcompile-stencil-x64` | `src/arch/x64.rs`, `src/arch/x64/` |
| `fastcompile-stencil-a64` | `src/arch/aarch64.rs`, `src/arch/aarch64/` |
| `fastcompile-stencil-tier` | `src/tier.rs`, `src/tier/`, requests 1 to 3 (codegen, link) |
| `fastcompile-stencil-diff` | `src/tests.rs`, `src/tests/` |

`fastcompile-stencil` keeps `src/lib.rs`, `src/arch.rs`, `Cargo.toml` and this file.

## Hooks requested

1. **codegen** (`crates/blaze-codegen/src/{lib.rs,settings.rs,content.rs}`):
   `Backend::Stencil`; `Toolchain::stencils: Option<String>` (the library id, hashed into
   `Settings` like `llvm`); `Target::Counter(InstanceKey)`.
2. **codegen** (`crates/hir-ty-codegen/v2/backend.rs`, `Cargo.toml`, `unwind.rs`,
   `fragment.rs`, `convert.rs`): `Prepared::Stencil`, built as Cranelift's (with
   `colocate`), and emitted through `blaze_stencil::compile` after `float_pair::lower`. Its
   `Code` is converted to `fragment::Code`, with `FunctionUnwind::from_rows` and
   `from_unwind_info` no longer gated on `llvm`, the LSDA from the call sites, and
   `RelocName::Counter` mapped to `Target::Counter`. Folds do not apply (`masked` gives
   `None`). An error is the unit's error.
3. **link** (`crates/blaze-link`): place `Target::Counter` cells, 16 bytes each and
   8-aligned, in one writable region of the image, with the region's address and each
   function's cell readable by the daemon (in-process, through blaze-inproc's daemon view).
4. **proof** (`harness/v2/corpus.py`, `engine.py`): a `backend` axis, so each corpus program
   is run with the dev tier forced to `stencil` and to `cranelift` and the outcomes are
   compared. Each run reports what it has without the other, which keeps the reference
   comparison with cargo as it is.
5. **coordinator**: the workspace dev-dependencies `cranelift-interpreter = "0.136"` and
   `cranelift-reader = "0.136"`, the build dependency `blake3` for `blaze-stencil` (already a
   workspace dependency), and
   `LLVM_SYS_231_PREFIX` set for every build of blaze (now needed by `blaze-stencil`'s build
   script as well as the `llvm` feature).
