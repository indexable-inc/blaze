---
status: plan
summary: LIR, blaze's own low-level IR, replaces Cranelift IR as the one input of code generation (Andrew's decisions, 2026-11-09) - its level (after layout, ABI and drop elaboration, before instruction selection), columnar encoding, types and operations, how it carries Rust's facts (rustc's set: noalias, nonnull, alignment, ranges, provenance, atomic orderings, volatile; no TBAA) as promises a consumer may drop and meanings it must keep, intrinsics and SIMD, unwinding, debug info, inline asm, references by place, its key and its blaze-format record; its consumers (stencils keyed on LIR ops, LLVM lowered from LIR directly, blaze's wasm emitter; Cranelift left only inside Wasmtime); LLVM's own allocations routed through blaze's allocator with a context per compile and charged to the unit's task; const eval staying on MIR; why a new IR and not lowered MIR, against codegen_ssa, CLIF, LLVM IR, GIMPLE/RTL, Zig's AIR, Go's SSA and MLIR; the migration straight to LIR with no bridge (lanes written in parallel, one switch gated on blaze building blaze and the real projects passing, then the deletions); ranked risks; the decisions that settled its open questions (no TBAA, no stored LIR, no extra facts and no folding until after the switch).
date: 2026-11-09
---
# LIR: blaze's low-level IR

Andrew's decisions (2026-11-09):

1. Code generation stops taking Cranelift IR (CLIF) as its input and takes blaze's own low-level
   IR, LIR.
2. LLVM release code is generated from LIR directly, LIR to LLVM IR, never through CLIF.
3. LLVM's own allocations (C++ `new` and `malloc`, today in the system's malloc zone and
   invisible to blaze's accounting) go through blaze's allocator and are charged to the unit's
   task ([below](#llvms-memory)).

The rest of this note follows from them. 

One builder lowers MIR to LIR once: layout, the
calling convention, drops, unwinding, intrinsics. Every producer of code reads LIR: the
copy-and-patch stencils for dev builds, LLVM for release builds and tier-up, and blaze's wasm
emitter for proc macros. Cranelift is left only inside Wasmtime, which compiles the macros'
wasm (rule `proc-macros-on-wasm`, [AGENTS.md](../../../../AGENTS.md)).

Chosen: a new IR in its own crate (`crates/blaze-lir`). It is SSA with block parameters, laid
out as columns, and machine-typed with a real pointer type. Rust's facts ride on it as flags
and attribute rows. Losing alternative, in one line: a lowered phase of MIR would be one IR
fewer, but every consumer would need the front end's database and MIR's per-body key
([below](#a-new-ir-not-lowered-mir)).

## What is wrong with CLIF as the input

**One builder, five readers of its CLIF.** `crates/hir-ty-codegen/mod.rs` (17,234 lines) is the
IR builder. `FunctionCx` (mod.rs:3654) lowers one monomorphized MIR body in
`MirPhase::Runtime`. `build` (mod.rs:2790) and `compile_as` (mod.rs:2464) drive it. The ABI
comes from `PassMode` (mod.rs:484), `pass_mode` (mod.rs:4260) and `fn_abi` (mod.rs:4384).
Calls, drops and pads are lowered in `terminator` (mod.rs:9489), `invoke` (mod.rs:15174) and
`emit_pads` (mod.rs:15301), and intrinsics in `intrinsic` (mod.rs:11974) and `llvm_intrinsic`
(mod.rs:14184). It uses about 130 CLIF opcodes; `store`, `load` and `iconst` are half of all
its calls. Its `ir::Function` is read by five things:

| reader | where | what it works around because the input is CLIF |
|---|---|---|
| Cranelift | `emit_with` (mod.rs:3125), `v2/backend.rs` `compiled` | n/a |
| stencils | `blaze_stencil::compile_to` (`crates/blaze-stencil/src/lib.rs:153`), via `v2/backend.rs` `stenciled` | Cranelift's ABI ported into the glue (`arch/*/abi.rs` from cranelift-codegen 0.136.2), `float_pair::lower`, .clif test corpora |
| LLVM | `llvm.rs` `emit`/`compiled`, `llvm/lower.rs` (3,084 lines) | pointers recovered from `i64` by dataflow (`Flow`, `addresses`, `pointerish`); over-alignment recovered by pattern (`realigned`); provenance repaired after inlining (`llvm/rebase.rs`, 770 lines, and the `REBASED` pipeline); `memcpy`/`_Unwind_Resume`/`memcmp`/asm wrappers recognized by the numbers in their `UserExternalName`s |
| wasm | `wasm::lower` (`wasm.rs:298`, 3,029 lines, plus `wasm/v128.rs`) | the instructions Cranelift lacks, passed as calls of names in `INSTRUCTION` |
| fold and shape | `v2/fold.rs` `masked_ir`/`merge`, `v2/shape.rs` (`VERSION` 10) | a canonical form rebuilt from Cranelift's own `Hash`, names replaced by place |

**Facts LLVM optimizes with are lost.** What reaches LLVM today is little, and mostly side
channels around CLIF:

- **Parameter facts:** `fragment::Pointee { unique, frozen, size, align }` (fragment.rs:803),
  computed by `FunctionCx::pointees` (mod.rs:6670). It covers only references to plain data
  (mod.rs:6716 to 6743), and no `Box`. `mark_pointees` (llvm/lower.rs:1009) applies it.
- **Signature attributes:** `sret` with `captures(none)`, `byval` with `align`, and
  `zeroext`/`signext`, from `ArgumentPurpose` and `ArgumentExtension` (`mark_signature`,
  llvm/lower.rs:1038 to 1068).
- **`inbounds` on the `offset` intrinsic's add only:** a bit smuggled in the instruction's
  `SourceLoc` (`unwind::seh::in_bounds`, mod.rs:12472, read at llvm/lower.rs:1946).
- **`nounwind memory(none)` on asm calls**, and PGO branch weights (`weigh`, llvm/lower.rs:714).

Everything else is gone:

- Every memory access the builder makes is `MEM = MemFlagsData::new().with_notrap()`
  (mod.rs:3976), so to LLVM it is `align 1`. Only `tier.rs`'s own IR is `aligned`.
- No `!range` reaches LLVM for `bool`, `char`, enum tags or `NonZero`. Neither does `!nonnull` or
  `!align` on a loaded reference, or `!noundef`.
- No `nsw`/`nuw`/`exact` reaches LLVM for `unchecked_*` or `exact_div`, and no `inbounds` for
  place projections or `ptr::add` (only `offset` has it, through the bit above).
- No `lifetime.start`/`end`: `overlap.rs` re-derives slot sharing.
- **Atomic orderings.** "Cranelift's atomics are always sequentially consistent"
  (mod.rs:12364), so LLVM is given `seq_cst` for every `Relaxed`, `Acquire` and `Release`
  (llvm/lower.rs:1612 to 1712). The one exception is the tier counters' `Monotonic` add.
  `compiler_fence` becomes a full `seq_cst` fence (mod.rs:12438, llvm/lower.rs:1634), not a
  single-thread one. On AArch64 a `Relaxed` load becomes an `ldar` and an RMW its `…al` form,
  and every reordering the weaker order allowed is lost.
- **Volatile.** `VolatileLoad` and `VolatileStore` are built as plain copies (mod.rs:12441),
  since CLIF has no volatile. LLVM may then merge or delete them. That is a correctness gap
  (MMIO, `black_box`-style code), not only a slower program.

**Workarounds for what Cranelift lacks, paid on the LLVM path too.**

- **Assembled stubs.** NEON, x86, AVX-512 and SVE intrinsics become calls of stubs assembled
  from text, with operands through memory: `neon.rs`, `x86.rs`, `avx512.rs`, `sve.rs`, and
  `asm.rs` (`neon_function`, `x86_function`, `avx512_function`). This happens even when LLVM
  compiles the function and has the intrinsic.
- **FMA.** `fma.rs` (1,716 lines) re-derives LLVM's own FMA choices.
- **Generic SIMD.** `wasm_simd.rs` admits a generic SIMD op only where Cranelift's x86 lowering
  has it on SSE2, so wider ops go lane by lane even when LLVM compiles them.
- **f16.** `f16_f128.rs` widens `f16` arithmetic to calls for every consumer.
- **Windows float pairs.** `float_pair.rs` returns a pair of floats under `windows_fastcall` as
  two integers. The Cranelift and stencil paths run it (backend.rs:234, `v2/backend.rs`
  `compiled` and `stencil_ir`) and the LLVM path does not, so stencil and LLVM code disagree on
  where such a pair is returned. That is a tier-up
  boundary to check, not yet a seen failure.

**A native Cranelift backend is still built.** v2's planner already gives native code only to
stencils or LLVM (`crates/blaze-codegen/src/plan.rs`: "Native code is stencils or LLVM's,
never Cranelift's"). Native Cranelift code is still made in two places, both v1:

- v1's `Backend` (`hir-ty-codegen/backend.rs:112`, `produce` at :199, `BLAZE_BACKEND`). Its
  default is `cranelift`, and it sends bodies over `LLVM_LIMIT` or with non-plain handlers to
  Cranelift.
- v1's `tier.rs`, whose level 1 is Cranelift.

v2's `emit_lent` and `v2/backend.rs` `compiled` serve `Backend::Cranelift` units. The planner
gives that backend only to wasm32 units (plan.rs:380), where `emit_with` returns into
`wasm::lower` before any Cranelift runs (mod.rs:3135). So `Backend::Cranelift { speed }` is the
wasm emitter's name, and native Cranelift units exist only in the driver's tests.

**Two codegen front ends, precisely.** The builder in mod.rs is one, shared. The duplicates are
the drivers around it, and v2's reference walk:

- **v1 drivers:** `compiled_root` (mod.rs:1007), `root_code` (mod.rs:1063), `instance_code` with
  `Detach::No`, `Backend::emit`, `tier.rs` and `fncache.rs`'s use of `compiled_root`.
- **v2 adapter:** `v2/backend.rs` `built_function`, `Backends`.
- **v2's reference walk.** `v2/refs.rs` and `v2/refs/intrinsics.rs` (4,292 lines) list an
  instance's references by re-running the builder's resolution on a `FunctionCx` that builds no
  code ("the lowerings that build IR are mirrored here"). `built_function` checks the mirror by
  count: `walked.len() != refs.len()`.

## What LIR is

### Level

LIR is one monomorphized function, after these, all done once by the builder:

- **Layout:** `rustc_abi` layouts from `db.target_data_layout`.
- **The calling convention:** `PassMode`/`fn_abi`, as rustc's `FnAbi`.
- **Drop elaboration and drop glue:** `drop_value`, `drop_glue`, `drop_ladder`.
- **Panics, overflow and UB checks:** under the unit's `Build` (`v2/build.rs`).
- **Statics, thread-locals, vtables and `#[track_caller]` locations.**

It comes before instruction selection, register allocation and frame layout, which are each
consumer's. A local whose address is taken, or that is not a scalar, is a stack slot. Every
other value is an SSA value. Places are gone: a field is a pointer plus a byte offset, and a
discriminant read is loads and compares. LIR names no Rust type, so a consumer needs nothing
of the front end.

### Encoding

The encoding follows [data.md](data.md): Zig's tag plus one word, lists in CSR, and hot and cold
columns apart. One function is these columns, on the compile's frame
(`blaze_mem::frame::task(Tag::Codegen, ..)`, [memory.md](memory.md)) while it is built, and the
same columns in its record when it is kept ([below](#keys-shape-and-the-record)):

| column | width | what |
|---|---|---|
| `ops` | `u8` | the operation (`lir::Op`) |
| `flags` | `u8` | what fits in a byte, per op (`Op::info` says which): promises (`nsw`, `nuw`, `exact`, `inbounds`, `in_range`, `nonzero`, `disjoint`), meanings (`volatile`, the atomic ordering, a fence's scope), hints (`cold`) |
| `tys` | `u16` | the result type (`lir::Ty`) |
| `data` | `u32` | one operand, an immediate, or an index into `extra` |
| `extra` | `[u32]` | operand lists, wide immediates, block targets with their arguments |
| `attrs` | `u32`, cold | 0, or a row of the memory-attribute table (`MemAttrs`) for a load, store, atomic or call |
| `locs` | `u32`, cold | the source location: an index into the function's span table, 0 for none |
| `block_starts` | `u32` | per block, in layout order, its first instruction |
| `block_kinds` | `u8` | normal, cold, `cleanup`, `catch`, `terminate` |

- **A value is the index of the instruction that makes it.** A block's parameters are its
  leading `param` instructions, so values are numbered in layout order (each block's
  parameters, then each instruction's result), the type of any value is `tys[v]`, and two
  builds of one function are the same bytes with no renumbering pass.
- **Results.** An instruction has at most one result. A call returning several values, and an
  overflow op (`uadd_overflow`), yield a compound that `project k` instructions read.
- **The tables** beside the instructions are columns too, in CSR: signatures with their
  parameters and parameter facts, memory attributes, ranges, references, the function's data
  and its relocations, intrinsics, slots, `asm` blocks, spans, variables, scopes and inlined
  bodies. The list of columns is one macro (`columns!`, blaze-lir `func.rs`) that `Func`,
  its borrowed `View` and the record are all made from, so they cannot drift.
- **Size budget**, held by `const` asserts as data.md asks: 8 bytes hot per instruction, plus its
  `extra`. CLIF's `InstructionData` alone is 16 bytes, before the DFG's lists.

### Types

`lir::Ty` (`u16`):

- **Integers:** `i1` (comparison results and `bool` as a value), `i8`, `i16`, `i32`, `i64`, `i128`.
  The arbitrary widths of `#![blaze::arbitrary_ints]` stay in their storage integer, normalized
  by the builder (`arbitrary_int.rs`), so LIR has no `iN`.
- **Floats:** `f16`, `f32`, `f64`, `f128`, with their arithmetic as ops. A consumer that has no
  instruction for one gets calls from the legalizer ([below](#passes-and-legalization)).
- **`ptr`:** the target's pointer, distinct from its integer. This is what carries provenance.
- **Fixed vectors:** `<n x T>` of any power-of-two lane count, up to 512 bits.
- **Scalable vectors:** `<vscale x n x T>` for SVE.
- **Register aggregates:** `[n x T]`, only in signatures, for the C ABI's homogeneous aggregates
  and cast targets (rustc's `CastTarget` and `Uniform`), so the consumer assigns registers by the
  target's rule. Today `PassMode::Uniform { skipped }` fakes that assignment with dummy
  parameters.

### Operations

The vocabulary starts as the 130 CLIF opcodes the builder uses. Then CLIF's "every address is
an integer" set is replaced by pointer operations, and the operations CLIF lacks are added:

- **Integer and float:** `add` `sub` `mul` `udiv` `sdiv` `urem` `srem` `and` `or` `xor` `shl` `lshr`
  `ashr` `rotl` `rotr` `icmp` `select` `zext` `sext` `trunc`; `clz` `ctz` `popcnt` `bswap` `bitrev`;
  `{u,s}{add,sub,mul}.overflow`, `{u,s}{add,sub}.sat`, `umulhi`/`smulhi`; `fadd` `fsub` `fmul`
  `fdiv` `frem` `fneg` `fabs` `fcopysign` `fma` `sqrt` `floor` `ceil` `trunc` `nearest` `fmin.num`
  `fmax.num` `fmin.ieee` `fmax.ieee` `fcmp`; conversions with saturating and trapping forms;
  `bitcast`.
- **Pointers:** `ptr_add` (pointer plus a byte offset, `inbounds` and `nuw` as flags);
  `ptr_addr` (the address, no provenance taken); `expose` (`expose_provenance`); `from_exposed`
  (`with_exposed_provenance`); `ptr_with_addr`; `null`; `func_addr`, `data_addr` and `tls_addr` of a
  reference.
- **Memory:** `load` and `store` (with `MemAttrs`); `memcpy`, `memmove`, `memset` and `memcmp` as
  operations, not libcalls recognized by name; `slot_addr`; `slot_live` and `slot_dead` (from
  `StorageLive` and `StorageDead`); `atomic_load`, `atomic_store`, `atomic_rmw`, `cmpxchg` (strong
  or weak, with success and failure orderings) and `fence` (with its scope: the thread or the
  system), each with its ordering.
- **Vectors:** `splat`, `extract`, `insert`, `shuffle` (a mask in `extra`), `swizzle`, `reduce.*`,
  `bitmask`, `select` by lanes, `widen` and `narrow`, `load`/`store` with a mask, `gather`,
  `scatter`, and every integer and float op lane by lane.
- **Control:** `jump`, `br` (with `likely`/`unlikely` as `expect_true`/`expect_false` hint
  flags, as rustc gives LLVM), `switch` (MIR's `SwitchInt`; LLVM's `switch`, a stencil jump
  table), `return`, `unreachable`, `trap` (with a code), `call`, `call_indirect`, `invoke` and
  `invoke_indirect` (a call with an unwind edge, which ends its block), `tail_call` and
  `tail_call_indirect` (`become`, [tail-calls.md](tail-calls.md)), `asm_br`
  (an `asm!` with `label` operands), `resume`.
- **Facts as instructions:** `assume` (`core::intrinsics::assume`, `assert_unchecked`); `freeze`
  (where the builder needs a value LLVM may not treat as poison).
- **Target and machine:** `target_intrinsic` (an `llvm.*` name from `core::arch`, by index into the
  function's intrinsic table, with its signature); `asm` ([below](#inline-assembly));
  `frame_address`, `return_address` and `stack_pointer` (what `get_frame_pointer` and
  `get_return_address` are now).

A per-op table (`Op::info`) gives arity, results, the flags each op may carry and which operands
take immediates. The crate has 146 operations; the table's code is part of the record's schema,
so a change to any operation's operands is a new schema. `lir::verify` checks every function against it, along with dominance and the
block-argument types: in debug builds always, in release builds behind a check mode.

### Semantics: defined by default, facts are opt-in UB

A flag is one of three kinds, and the kind says what a consumer may do with it:

- **Promises** are opt-in UB, and a consumer may drop them.
- **Meanings** must be honoured or strengthened, never dropped. `volatile` stays volatile for
  any consumer that optimizes. An atomic's ordering may only be raised, up to `seq_cst`, which
  is also the ordering of an atomic op that names none. A single-thread fence may only become a
  full fence.
- **Hints** (`cold`, branch weights) may be dropped.

Every operation without promises is fully defined, the way CLIF's are:

- A shift masks its amount.
- Division by zero traps with its code (`int_divz`).
- `fptosi` saturates.
- A load reads the bytes there.

Each promise is one the builder proves from Rust's semantics. Breaking it is UB, as in LLVM's
poison model:

- `nsw`/`nuw` for `unchecked_add`.
- `in_range` on a shift after its overflow check.
- `inbounds` on `ptr::add` and field projections.
- `exact` for `exact_div`.
- `nonzero` on a division whose divisor was checked.
- A `MemAttrs` range on a load of `bool`.

So a consumer may ignore any promise or hint and still compile a correct program, because
dropping a promise refines the program. Stencils and wasm ignore every promise and hint, keep
`volatile` (they never merge or remove an access anyway) and strengthen orderings. LLVM maps all
three kinds. The builder can add a promise later without changing what any promise-free
program means, so a fact can be landed, measured and reverted by itself.

The one exception is memory. Bytes may be uninitialized and pointers carry provenance, as in
Rust's abstract machine, which rustc maps onto LLVM's. A load of uninitialized bytes as a
scalar is UB in Rust and poison in LIR. Every consumer but LLVM gives it the bytes that are
there.

### Rust's facts, and where each goes

LIR carries exactly what rustc's `rustc_codegen_llvm` tells LLVM for the pinned nightly, and no
more by default (rule `superset-of-rust`: a program that runs correctly when rustc builds it
must not be miscompiled by a stronger claim). The rules are ported from rustc's source by
reading it, never by running it: `rustc_ty_utils::abi`'s `arg_attrs_for_rust_scalar`,
`rustc_codegen_llvm::abi`, `builder.rs`'s load metadata, and `rustc_codegen_ssa::mir`. Each rule
is a function of the builder named after the rustc function it ports.

| Rust fact | where the builder knows it | LIR | LLVM |
|---|---|---|---|
| `&T`, `&mut T` parameters: non-null, aligned | the layout of `T` | parameter attributes | `nonnull`, `align` |
| dereferenceable: `&T` with `T: Freeze`, `&mut T` with `T: Unpin` (rustc gives the others, and `Box`, a pointee size of 0) | `is_freeze`, `Unpin` (`pointees`, mod.rs:6670, for plain data today) | `deref(n)` | `dereferenceable(n)`, `dereferenceable_or_null(n)` for `Option<&T>` |
| `&mut T` with `T: Unpin`; `&T` with `T: Freeze`; `Box<T, Global>` with `T: Unpin` | the same, and the allocator | `noalias`; `readonly` for `&T` alone | `noalias`, `readonly` |
| valid range of a scalar argument or return (`bool`, `char`, enums, `NonZero`) | `Scalar::valid_range` | `range(lo, hi)` | `range` attribute, `noundef` |
| a loaded scalar with a valid range | the place's layout | `MemAttrs.range` | `!range`, `!noundef` |
| a loaded reference | the place's type | `MemAttrs.nonnull`, `.align` | `!nonnull`, `!align`, `!noundef` |
| alignment of every access | the place's layout (1 under `repr(packed)`) | `MemAttrs.align` | `align n` |
| vtable slots | vtable loads | `MemAttrs.invariant` | `!invariant.load` |
| `unchecked_*`, the non-overflowing arm of checked arithmetic | the intrinsic, the `Assert` | `nsw`/`nuw` | `nsw`/`nuw` |
| `exact_div`, `unchecked_shl`/`shr`, `disjoint_bitor` | the intrinsic | `exact`, `in_range`, `disjoint` | `exact`, no mask, `or disjoint` |
| field projections, `ptr::add` (a `usize` count) | the projection, the intrinsic | `ptr_add inbounds nuw` | `getelementptr inbounds nuw` |
| `ptr::offset` (an `isize` count) | the intrinsic | `ptr_add inbounds` | `getelementptr inbounds` |
| `wrapping_offset`, `wrapping_add` on pointers | the intrinsic | `ptr_add` | `getelementptr i8` |
| atomic orderings, weak `compare_exchange` | `AtomicOrdering` of the intrinsic | the op's ordering | the ordering, `cmpxchg weak` |
| `read_volatile`, `write_volatile` | the intrinsic | `volatile` | `load volatile`/`store volatile` |
| `StorageLive`, `StorageDead` | MIR | `slot_live`/`slot_dead` | `llvm.lifetime.start`/`end` |
| `assume`, `assert_unchecked`, `likely`, `unlikely`, `cold_path`, `#[cold]` callees | the intrinsic, `cold_fn` (mod.rs:1915) | `assume`, branch weights, cold blocks | `llvm.assume`, `!prof`, `cold` |
| `fadd_fast` and the rest, `*_algebraic` | the intrinsic | float flags | fast-math flags |
| `-> !`, nounwind (panic=abort, `extern "C"` without `-unwind`), `#[inline]`, `#[track_caller]` | the signature, `Build::aborts`, `fragment::Hint` | function attributes | `noreturn`, `nounwind`, `inlinehint`/`alwaysinline`/`noinline` |

**No TBAA.** Rust has no type-based aliasing: a program may read an `f32` through a `*const
u32`, and rustc emits no `!tbaa`. The type information LLVM gets is what the table gives:
alignment, dereferenceability, ranges, and `noalias` from references. A stronger alias model
(scopes for bodies blaze's MIR inliner builds in, as LLVM's inliner makes them from `noalias`
parameters) comes after the switch, each fact its own measured change (Andrew, 2026-11-09).

### Memory, pointers and provenance

`ptr` is a type of its own, so LLVM is given pointers and `getelementptr`s as rustc gives them,
and the recovery of pointers from integers is deleted: `Flow`, `addresses`, `pointerish`,
`byte_offset` and `same_but_pointers` from llvm/lower.rs, and all of `rebase.rs`. The strict
provenance APIs map one to one:

| Rust | LIR | LLVM |
|---|---|---|
| `addr` | `ptr_addr` | `ptrtoint` |
| `expose_provenance` | `expose` | `ptrtoint` |
| `with_exposed_provenance` | `from_exposed` | `inttoptr` |
| `without_provenance` | `ptr_add null, n` | `getelementptr i8, null, n` |
| `with_addr` | `ptr_with_addr` | `getelementptr` by the difference |

A transmute from a pointer to an integer is `ptr_addr`. One from an integer to a pointer is
`from_exposed` at first, which is more defined than Rust asks and so refines. Whether to lower
it as `without_provenance`, as the standard library describes it, is decided by porting rustc's
lowering of it, like every other fact.

A stack slot has a size, an alignment (any power of two, so `realigned` is deleted) and a kind:
a named local for debug info, a spill of an argument passed on the stack, or a temporary.

### Calls and the ABI

The ABI is decided once, in the builder, by `PassMode` and `fn_abi` as today. A LIR signature
is already lowered:

- **Parameters and returns:** a list of machine types.
- **Per-parameter ABI:** zero or sign extension, `sret`, `byval(size, align)` for C's on-stack
  aggregates (`PassMode::OnStack`), and the attribute row of the facts above.
- **Calling convention:** `C` of the target (System V, Win64, AAPCS64 or Apple's), or `Wasm`.

A Rust-ABI function uses its target's C convention over its lowered signature, as it does today
through Cranelift's conventions. So stencil code, LLVM code and C call each other with no
adapter, and a tier-up is one slot store. What the builder decides stays in the builder:

- the `i128` rules (`lone_register_i128`, `stranded_i128`), two-word returns, `Spread` short
  vectors, the hidden pointer for a pair returned on wasm;
- `#[track_caller]`'s trailing location, and variadics (`Variadic`, `VariadicRegisters`,
  `HomedRegisters`).

The reference for where a signature's arguments go is LLVM's lowering of it, since release code
and C interoperate through LLVM. Today the reference is Cranelift's `isa/*/abi.rs`, which LLVM
matched only through `enable_llvm_abi_extensions`. The stencil glue is held to LLVM's assignment
by test ([below](#stencils)), and `float_pair.rs` is deleted: a pair of floats is returned in
`xmm0` and `xmm1` by every consumer, as LLVM and rustc return it.

### Intrinsics and SIMD

- **Rust intrinsics** (`core::intrinsics`) lower to LIR operations in the builder, as now.
  The `v2/refs/intrinsics.rs` mirror is not ported ([below](#references-by-place)).
- **Generic `simd_*`** lower to LIR vector operations of the type's real width, on every target.
  Which widths and ops a target executes natively is the consumer's question. The per-ISA
  admission in `wasm_simd.rs` (`admits`, `Vectors::{Sse2, Neon, Wasm}`) moves out of the builder
  into legalization.
- **`core::arch` intrinsics.** Each is an `extern "unadjusted"` function with `link_name =
  "llvm.*"`, and each becomes `target_intrinsic` with that name and signature. LLVM emits the call
  of that intrinsic, as rustc does. Stencils get a stencil per intrinsic, generated by LLVM from
  that same call ([below](#stencils)). `neon.rs`, `x86.rs`, `avx512.rs`, `sve.rs`, `fma.rs`, the
  stub paths of `asm.rs` (`neon_function`, `x86_function`, `avx512_function`, `call_assembled`)
  and the data tables `neon-fma-llvm.txt` and `sve-llvm.txt` are deleted.
- **SVE values** are `<vscale x n x T>` values. Only a consumer without scalable registers keeps
  today's register image in memory.

### Unwinding

- **Calls that can unwind:** `invoke` and `invoke_indirect`, a call with an unwind edge to a
  block of kind `cleanup`, whose one parameter is the exception. An invoke ends its block; its
  result is a value of the block it returns to, whose one predecessor it is. A `call` cannot
  unwind.
- **`catch_unwind`:** the `try` intrinsic has a `catch` edge.
- **`UnwindAction::Terminate`:** a `terminate` edge, a cleanup that calls `panic_cannot_unwind`.
- **`UnwindResume`:** `resume`.
- **`panic = "abort"`:** no edges.

That is the whole model in LIR, the same on every target. How a target unwinds is the
consumer's:

- **Stencils** keep today's `try_call` glue and LSDA (`lsda::write`), and on Windows today's
  guarded call and SEH tables (`unwind/seh.rs`).
- **LLVM** keeps today's landing pads with blaze's personalities (llvm/lower.rs `personality`,
  `unwind::seh::PERSONALITY`).

The guarded-call trick moves out of the builder (`guarded_call`, mod.rs:15274, and the SEH bit in
`SourceLoc`, `seh::guarded`) into the stencil glue for Windows. `plain_handlers` and the
`LLVM_LIMIT` routing go with v1's `Backend`. `unwind.rs`, `unwind/lsda.rs`,
`unwind/gcc_except_table.rs` and `unwind/seh.rs` are unchanged: they read what a consumer
reports.

### Debug info and line tables

Each instruction's `locs` entry indexes the function's span table, today's `Recorder.spans` and
`Function.spans: Vec<SpanRef>`. These are position-free `SpanRef`s, so an edit elsewhere in the
file changes no byte of the LIR. Named locals and their scopes are two cold tables beside the
function (`variables`, `scopes`), today's `side::Variable` and `side::Scopes`. Each names its slot.
`promote` and the slot-sharing pass update them, as they update `Recorder.named` now.

A consumer gives back what it gives today:

- `LineRow`s (`LineRow::of_ranges`) of `(offset, loc)`.
- `SlotPlaces`: where each named slot is in its frame. The stencil patcher knows every slot's
  offset. LLVM gets `DILocalVariable` and `dbg.declare` on the slot's `alloca`.

`debuginfo.rs` and `debuginfo/` (DWARF, split DWARF, CodeView) are unchanged.

### Inline assembly

`asm!` is an `asm` instruction naming a row of the function's `asm` table. A row holds:

- the template pieces;
- each operand: `in`, `out`, `inout` or `lateout`, with a register or class, `const` already
  folded into the text, `sym` as a reference by place, and `label` as a block target;
- the options and `clobber_abi`.

LLVM always gets real inline assembly. That is the form `asm::inline`/`keep_inline` builds
today, so the wrapper path no longer exists for LLVM. Stencils need a wrapper, a small function
the block is assembled into (today's `asm::wrapper`, asm.rs:836, called from
`FunctionCx::inline_asm`, mod.rs:7887), which the stencil glue now
builds from the same row and caches as `asm::assemble` does. Naked functions and `global_asm!` are
not LIR: they stay `Built::Assembled` and `v2/global_asm.rs`. The MSVC `throw_exception` case
(`is_seh_throw_info`) stays in the builder.

### References by place

A function's `refs` table lists what it refers to:

- functions, statics and thread-locals, each by its place in `Described::refs`;
- its own data (constants, `file!()` texts, panic locations) by content;
- target intrinsics by name.

This is today's `Places` and `fragment::Requests`. A call or address operand names a row. So a
LIR names no dense id of the run and no callee's identity, and two instances whose code differs
only in what they call (`Vec<A>::drop` and `Vec<B>::drop`) are byte-equal LIR. That is the
property `v2/shape.rs` rebuilds from CLIF today, and LIR has it by construction.

The walk that lists an instance's references before any unit compiles it (`Described::refs`,
the union over every setting a unit may build it under: `Build::WALK` and the two of
`Build::WALKS_INLINED`) stays. What changes is who runs it: the one builder, with
`lir::Builder`'s requests-only sink, which records requests and makes no instructions. So the
walk is the lowering itself, and `v2/refs.rs`'s and `v2/refs/intrinsics.rs`'s hand-kept mirror
of it, and the `walked.len() != refs.len()` check, are not ported.

### Keys, shape and the record

**What the LIR depends on** is split out of `Spec` at the type level:

- `LirSpec` is what the builder reads: `Build` (`v2/build.rs`), target and CPU features,
  frame pointers, debug-info level, the sanitizer set, overflow checks, the proc-macro bridge,
  and the devirt world.
- `EmitSpec` is what a consumer reads: the backend, LLVM's level, the stencil library id,
  counting, fuel, and PGO.

The builder is handed a `LirSpec` and cannot read a field its key lacks (rule
`keys-cover-every-input`).

- **`LirKey`** = `H("blaze lir 1", lir::VERSION, instance_identity(..), built_in, LirSpec,
  Toolchain::id)`. The compile identity is [code-identity.md](code-identity.md)'s. It leaves out
  what the template key covers instead, so the LIR's key takes that part too: `built_in` is
  the code hashes of every body the LIR builds in (`#[inline(always)]` callees and what MIR
  inlining took, `Function::inlined`), as `dev_template_key` and `release_template_key` key
  them, and `LirSpec` holds the devirt world. `Toolchain::id` covers the builder's own code
  (`CodegenCode`). Two values under one `LirKey` are a typed refusal.
- **`Shape`** (`blaze_codegen::backend::Shape::of`) hashes the LIR record's bytes as given,
  under `EmitSpec` and the architecture. The bytes hashed are the bytes kept. `v2/shape.rs`
  (its canonicalizer and its `namespace` table) is deleted, and `SHAPE_DOMAIN` moves to
  `"blaze v2 shape 2"`. Equal LIR compiles once per consumer, through `memo.rs` as now.
- **Template keys.** `dev_template_key` and `release_template_key` (blaze-codegen
  `driver.rs:388`, `:410`) keep their shape. A template is machine code by place
  (`Template`, `blob.rs` `BZT7`), unchanged.

**The record.** A kept LIR is one `blaze_format` buffer, the columns above as `Slice`s of one
`record!` root (`blaze_lir::persist::LirRoot`), as `hir-ty/src/mir/persist.rs`'s `MirRoot` keeps
a MIR body: the function's `Head` (its signature row, attributes and pointer width), then one
`Slice` per column, generated from the same `columns!` list as `Func` and `View`.

The schema hash covers every field, every table row's layout, `blaze_lir::VERSION` and the
operation table's code, so a LIR of another encoding reads as a schema mismatch, never as
another function. `lir::persist::read` checks the buffer as blaze-format does, then every tag,
length and index of the columns (`verify::structure`), so a reader may index the `View` it gets
without checking. The record is the in-memory form: a consumer reads a kept LIR in place, with
no decoding into another structure. When LIR is kept (after the switch, below) it is stored as
`blaze_cas::Kind::Lir` (a new kind), in the warm and cold tiers of [data.md](data.md) (columns
compress well: `ops` and `tys` are low-entropy).

**When a LIR is kept.** Never at the switch (Andrew, 2026-11-09): no build stores LIR, and the
record exists for the round-trip tests and `blaze explain`. After the switch, measured by
itself, a LIR may be kept where another build reads it:

- every function of a release unit, so ThinLTO-style imports (`blaze_codegen::imports`,
  `v2/release.rs` `summarize`/`emit`) can read a callee's LIR from the store instead of the
  `Export` held for one build;
- every function of a run that tiers up, so the daemon compiles a hot function, and the callees
  it builds in, at LLVM from their LIR.

A dev build that only makes stencil code keeps none: its template is what is reused.

**Round trips** (rule `persisted-state-round-trips`):

- `read(write(f)) == f` byte for byte, by a Hegel property over generated LIR.
- Once LIR is kept: a LIR read from the store compiles to the same bytes as a fresh build, in
  each consumer.
- `lir::print` and `lir::parse` round-trip the text form, which replaces the .clif corpora
  of the tests and is what `blaze explain` prints for a function.

### Passes and legalization

`crates/blaze-lir/src/pass/`. Each runs on LIR, once, in one order, before the shape is taken,
so every consumer sees one LIR:

- `promote`: today's `promote.rs`, slots written and read whole become values.
- `copies`: today's `copies.rs`, small `memcpy`/`memcmp` as loads and stores.
- `fold`, after the switch (Andrew, 2026-11-09): today's `v2/fold.rs` `masked_ir`/`merge` on
  LIR, its own measured change, which also decides whether it runs for every backend and not
  only wasm32. Masking a function is zeroing its immediates' `data` words, a column operation.
  Until it lands, a LIR build folds no functions, wasm32's included.

Consumer-side, after the shape, a deterministic function of the LIR, the consumer's `Accepts`
set and its library id:

- `legalize`: `f16`/`f128` arithmetic to calls (stencils, wasm), vectors wider than the
  consumer's registers to halves, a lane op the consumer lacks to lanes. This takes
  `f16_f128.rs`, the lane loops in mod.rs and `wasm_simd.rs`'s fallback out of the builder.
  LLVM accepts everything.
- `frame`: slots whose `slot_live`/`slot_dead` ranges never overlap share room, for stencils and
  wasm (today's `overlap.rs`). LLVM does its own stack colouring from the markers.

Deleted with no replacement:

- `float_pair.rs`, since the one ABI returns pairs where LLVM does;
- `blaze_stencil::tier::instrument`, since stencil code counts in its prologue and back edges
  and no Cranelift code is left to instrument;
- Cranelift's ISA selection (`isa_for`, `Isa::Large`, `Isa::FramePointer`).

## Who reads LIR

### Stencils

`blaze_stencil::compile_to(func: &lir::Func, target, sink)`. The patcher's passes
are unchanged in kind: scan, select, slots,
frame, size, emit, edges, finish. They read columns instead of matching `ir::InstructionData`.

- **Keying.** `Shape { op: lir::Op, ty: lir::Ty, aux, srcs, dst }`, packed by `Shape::key` as
  now, enumerated by `shape::each`, indexed by the PTHash table, under a new `shape::VERSION`.
  The fusions (`BrIcmp`, `SelectIcmp`, `LoadConfined`/`StoreConfined`) are LIR patterns.
  Promises and hints are not in the key, since stencils ignore them, so a fact never multiplies
  the library.
  `aux` gains the atomic ordering only where it changes the instruction (`aarch64_lse` acquire
  forms), and otherwise orders are strengthened to `seq_cst`, which refines.
- **One meaning of each op.** The generator (`build.rs`, `gen/`) stops writing each op's LLVM IR
  by hand (`gen/ops.rs`). It builds a one-instruction LIR function and lowers it through
  `blaze-llvm`'s per-op lowering, the code release builds use, then wraps it in the stencil
  convention (`preserve_none`, `musttail` to the continuation, holes as `!absolute_symbol`).
  So stencil code and LLVM code agree on every op by construction. Today they each agree with
  Cranelift only by test.
- **Target intrinsics** get stencils the same way: one per `link_name` the pinned `core::arch`
  declares for the variant (x86-64 baseline, AArch64, AArch64+LSE), operands in slots of 16,
  32 or 64 bytes. Their count and size are risk 5.
- **The glue's ABI** is held to LLVM's: for every signature shape (generated by Hegel: integer,
  float, vector, `i128`, `sret`, `byval`, `[n x T]`, variadics, `#[track_caller]`, on each OS),
  a test compiles a caller in stencil code and a callee by LLVM and the reverse, and checks that
  each side reads the values the other passed.
- **Tests** are blaze's own and the real projects'. blaze-stencil's tests check:
  - the library: every shape has a stencil, every hole a form `enc` patches, and no stencil
    has a stack, a call or a write of a reserved register;
  - each op's result on edge-first operands against the Rust operation it stands for
    (`u32::wrapping_shl`, `i64::checked_div`, `f32::min`), written in the test, so the
    expected value never comes from the lowering under test;
  - the ABI cross-calls above.

  `cranelift-interpreter`, `cranelift-reader` and the .clif corpora go. Whole programs are
  judged by their own tests, built by blaze.

`blaze-stencil` then depends on `blaze-lir` and no Cranelift crate. Its tests read LIR in the
text form (`lir::parse`). The real builder's output reaches stencils in hir-ty-codegen's tests
and the real projects, since a dev-dependency back on hir-ty-codegen would still be a cycle.

### LLVM

`crates/blaze-llvm` (moved from `hir-ty-codegen`'s `llvm.rs` and `llvm/`) lowers LIR to LLVM IR
directly through inkwell over `vendor/llvm-sys`. One LIR block is one LLVM block, and block
parameters are phis. `ptr` is `ptr` and `ptr_add` is `getelementptr i8`, so every address has
its base. Each fact maps by the [table](#rusts-facts-and-where-each-goes): parameter and
return attributes, load and store metadata, instruction flags, orderings, lifetime markers,
branch weights.

- **Kept as they are:** `release.rs` (summaries, `prelink`, `postlink`, `Facts`, `Given`),
  `pgo.rs`, `sanitize.rs`, `report.rs`, `emitted.rs` (the object read back into `Code`), and
  `vendor/llvm-sys/wrappers/passes.cpp`'s kept pass managers.
- **Deleted:** in llvm/lower.rs, `Flow`, `addresses`, `pointerish`, `realigned`, `byte_offset`'s
  guesswork, `memory_call`, the `Symbol::of_user` recognition of named numbers, and
  `mark_signature`'s recovery from `ArgumentPurpose`; all of `llvm/rebase.rs` and the `REBASED`
  pipeline; `llvm/lower/vector.rs`'s rebuilding of LLVM ops from Cranelift's lane semantics,
  since LIR's vector ops are LLVM's.
- **One crate, no front end.** `blaze-llvm` depends on `blaze-lir`, inkwell and llvm-sys, never
  on `hir-ty`. The daemon's tier-up and the stencil generator use it with no database.
- **What today's lowering gives LLVM is kept, and none of it is recovered any more:**
  `inbounds` on `offset`, the signature attributes, `nounwind memory(none)` on asm calls, PGO
  branch weights. Each becomes a LIR fact the builder states.

### LLVM's memory

LLVM allocates with C++ `new` and `malloc` (`SmallVector`'s `safe_malloc`, `BumpPtrAllocator`'s
`allocate_buffer`, every `Value`, `Type` and `Metadata`). Today those bytes go to the system's
allocator (the default malloc zone on macOS, glibc on Linux). blaze's global allocator
(`blaze_graph::arena::Counting<heaps::Heaps>`, `crates/blaze-engine/src/lib.rs`, mimalloc with a
heap per blaze-mem tag, `crates/blaze-engine/src/heaps.rs`) sees only Rust's allocations. So a
release unit's peak (`arena::account`, the plane's admission, `blaze_graph::admission`) and the
memory line leave LLVM out, though LLVM holds the most of any release task.

**Every C and C++ allocation goes through blaze's allocator.**

- **The entry points.** blaze-engine's binary defines, under its `native` feature, the malloc
  family (`malloc`, `calloc`, `realloc`, `free`, `posix_memalign`, `aligned_alloc`,
  `malloc_usable_size`/`malloc_size`) and C++'s replaceable `operator new` and `operator
  delete` (every size, alignment and `nothrow` form). Each is `Counting`'s count over
  `Heaps`: an allocation goes to the mimalloc heap of the running task's tag
  (`blaze_mem::frame::running_tag`) and is charged to the thread's account
  (`arena::current`); a free is taken off the tag `heap_of` names. A C block has no
  `Layout`, so its size is `mi_usable_size` both when it is made and when it is freed, and the
  two balance.
- **Linux.** The executable's definitions interpose glibc's for every image, so LLVM (linked
  statically through llvm-sys) and every C dependency use them.
- **macOS.** LLVM's references to `_malloc` and `__Znwm` bind to the executable's own
  definitions at static link. Replacing `operator new` also covers what libc++ allocates for
  LLVM. The default malloc zone is left to the system's own frameworks.
- **Blocks blaze did not make.** On both, `free` and `realloc` of a block another image's
  allocator made (`strdup` or `realpath` in libSystem, then freed by LLVM) is found by the
  page-map check `heaps.rs` already mirrors (`heap_of`, `_mi_page_map`), and handed to the
  system's `free`. Freeing it into mimalloc would corrupt both heaps.
- **Tests.**
  - Every entry point allocates and frees through `Counting`.
  - Foreign blocks are freed by the system.
  - The planted control: an LLVM compile run under `arena::account` has a peak of megabytes
    with the routing and near nothing without it (rule `test-the-cause`).

**A context per unit, disposed at its end.** Today `Machine` (llvm.rs) keeps one `Context` per
thread for 1,024 modules ("The constants and source locations of all of them stay in it"), so
what one unit made stays held, charged to whichever task made it, into the next thousand units.
Instead, each LLVM compile makes its own `Context` inside the unit's task and disposes of it
before the task's account closes: a `summarize`, an `emit`, a tier-up compile, an assembly.
Between `summarize` and `emit` the module is bitcode (`Kept`'s `own` and `export`), so nothing
of a context outlives its compile. The per-thread `Machinery` (the target machine and the kept
pass managers of `LLVMBlazeRunPasses`) is not tied to a context. It stays per thread, made
outside any account and charged to a new `blaze_mem::Tag::Llvm` (`Tag::COUNT` 23), whose share
of the machine is LLVM's budget.

**How admission sees it.**

- A unit's peak now includes everything LLVM held for it: what its context, its modules and its
  passes allocated, and what was freed when the context was disposed.
- `Estimates::observe` and blaze-history's per-key peaks predict the next compile of that key
  from it.
- Each LLVM compile runs as a frame task of `Tag::Llvm` (`blaze_mem::frame::task(Tag::Llvm, ..)`)
  inside the unit's task and its account. So its bytes are LLVM's in the memory line, and
  `blaze_mem::tag::over_budget` holds back LLVM work past LLVM's share before the machine's
  budget is reached.
This work is independent of LIR and lands first (lane `llvm-memory`).

### wasm, for proc macros

A proc macro is a `wasm32-wasip1` module, and so is
the code of a user's `--target wasm32-wasip1` build and of the wasm seed. That code comes from
LIR through blaze's own emitter: `wasm::lower` and `wasm/v128.rs` ported to read LIR,
moved to `crates/blaze-lir-wasm`. It does not come from LLVM, since the pinned LLVM has no
WebAssembly target (the `llvm-wasm` feature is off). Wasmtime then compiles the module with
its Cranelift: the one place Cranelift stays, and `cranelift_codegen::ir` is not seen outside
Wasmtime. The emitter ignores flags, and its atomics are wasm's (`seq_cst`).
`blaze_codegen::Backend::Cranelift { speed }` becomes `Backend::Wasm`, its true name.
`check_built`'s wasm rule (`v2/backend.rs`) names it.

### Cranelift, inside Wasmtime only

After the switch, Cranelift is in blaze only as Wasmtime's compiler. It runs in two places:

- in `blaze-macro-eval`'s wasm host, compiling each proc macro's `wasm32-wasip1` module (the
  module's machine code kept in the store, `src/wasm/code.rs`);
- in the wasm seed's runner (`blaze-seed-run`, `blaze-wasi`).

The vendored fork (`vendor/cranelift-codegen`, `=0.137.1`) stays pinned for Wasmtime alone. No
blaze crate names `cranelift_codegen` or `cranelift_frontend` but through `wasmtime`, and a
`.clippy.toml` ban holds that.

## LIR is not the const evaluator

Const eval stays on MIR, in the MIR evaluator (`hir-ty-mir/src/mir/eval.rs`: `Evaluator`,
`interpret_mir`), as rule `proc-macros-on-wasm` has it: the evaluator stays for const eval and
as the reference a macro's wasm output is tested against (`blaze-macro-eval`'s `tests/wasm.rs`).

- **Abstract memory.** Const eval runs on Rust's abstract machine: allocations with
  provenance, uninitialized bytes, alignment and validity checked. It detects UB as rustc's const
  eval and Miri do, and its errors match the pinned nightly's.
- **LIR has lowered all of that away.** LIR is per target and after layout, its pointers are
  addresses and its memory is bytes. A constant evaluated on LIR, or by running native code,
  would accept what rustc refuses and could not say why.
- **Speed comes from a faster MIR evaluator**: bodies pre-lowered once into the evaluator's own
  form, results cached by code identity (`hir_ty_identity::code_id`), and the evaluator's own
  hot paths. Never from evaluating LIR or native code.

LIR has no interpreter. Codegen's correctness is that of the programs blaze builds: real
projects built by blaze passing their own tests (rule `verify-on-real-projects`), plus blaze's
own tests of each part.

## A new IR, not lowered MIR

The options, against what LIR has to be:

- read by stencils with no front end;
- cheap to build and to scan twice;
- flat, so it can be hashed and kept;
- one lowering, so the ABI and drops are decided once;
- able to carry the facts;
- no heavy new dependency.

**rustc's `rustc_codegen_ssa`.** `rustc_codegen_ssa::mir` lowers MIR once, backend-agnostic,
through the `BuilderMethods` trait (about two hundred methods), with the ABI decided once by
`rustc_target::callconv::FnAbi`. LLVM and GCC implement the trait. Facts are builder calls,
which works well for LLVM. But nothing is materialized: no IR exists to hash into a shape, to
keep in the store, or to scan before emitting (the stencil patcher counts uses and last uses
first). And rustc's own Cranelift backend (`rustc_codegen_cranelift`) does not use it: it lowers
MIR a second time, which is the duplication blaze must not have. Taken: the ABI decided once
(`PassMode`/`fn_abi` are blaze's `FnAbi`), and `lir::Builder`'s surface shaped like
`BuilderMethods`.

**Cranelift's CLIF** is right for the patcher: SSA with block parameters, a small op set, every
op defined, a compact DFG. It was made for compiling wasm fast. It has:

- no pointer type, so addresses are integers;
- a `MemFlags` of a few bits (`aligned`, `readonly`, `notrap`, `can_move`, heap/table/vmctx
  regions), with no ranges, no `noalias`, no orderings and no volatile;
- no target intrinsics, no scalable vectors, and little past 128-bit vectors;
- a version pinned to a fork (`vendor/cranelift-codegen`, `=0.137.1`).

Taken: block parameters, defined-by-default semantics, the op vocabulary to start from.
Rejected: as the input, since side tables keyed by `ir::Inst` for every lost fact would be a
second IR bolted onto the first.

**LLVM IR** carries every fact and is what the facts mean (poison, attributes, metadata). As the
one IR it would make every dev compile pay for LLVM's in-memory IR: an allocation per
instruction, use lists, a context per thread. LLVM would have to be linked into every dev
compile path, where today stencils need it only when blaze is built. It has no stable
serialization but version-bound bitcode, and its pin has no WebAssembly target. Taken: the
facts' names and meaning, and the poison model for flags.

**GCC's GIMPLE and RTL.**

- **GIMPLE** is three-address SSA with types, where GCC's alias analysis runs (points-to, and
  type-based because C has strict aliasing).
- **RTL** is the machine level after `expand`, where the ABI and instruction patterns are
  applied.

LIR is a GIMPLE-level IR with the ABI already applied: low GIMPLE plus calling convention.
RTL's job (selection, allocation, the frame) is each consumer's. Taken: two levels with a sharp
line between them. Rejected: GIMPLE's type-based alias sets, which Rust's semantics forbid.

**Zig's AIR** is the closest in form: per function, a tag column, an 8-byte data column and a
`u32` extra array (the encoding data.md takes), read by the LLVM, C, x86_64, AArch64, wasm and
SPIR-V backends. But AIR is before layout and the ABI, so each Zig backend has its own ABI code
(`codegen/llvm.zig`'s and `arch/x86_64/abi.zig`'s), and a backend's ABI bug is its own. Blaze's
stencil and LLVM code call each other at tier-up, so that split cannot be afforded. Taken: the
encoding. Rejected: the level.

**Go's SSA** (`cmd/compile/internal/ssa`) is one IR from generic ops down to machine ops: rewrite
rules (`_gen/*.rules`) lower generic to `AMD64`/`ARM64` ops in place, and `expand_calls` applies
the ABI. Taken: target operations as operations of the same IR (`target_intrinsic`), and
legalization as table-driven rewrites in one place. Rejected: lowering to machine ops in the IR,
which is the stencils' and LLVM's work.

**MLIR**: dialects, progressive lowering, typed attributes, a verifier per op, an LLVM dialect.
It is a large C++ dependency, slow to build and to construct, and its generality (regions,
open attribute dictionaries) costs what the patcher's speed budget can't pay. Taken: typed
attributes per op class, and `lir::verify` per op from `Op::info`.

**A lowered phase of MIR.** This would extend `hir-ty-mir`'s MIR with a phase after layout
(as rustc's `MirPhase`), whose locals are slots and values and whose statements are LIR's ops.
One IR fewer, but:

- MIR's values are typed by `Ty<'db>`, interned in the front end's database, so every consumer
  (stencils, the daemon's tier-up, the wasm emitter) would need the database, and codegen would
  live in the front end's query crates (rule `semantics-in-front-end` keeps it out).
- MIR is per generic body, keyed by its body's identity (`hir-ty/src/mir/persist.rs`). Lowered
  code is per instance, keyed by compile identity, layout and settings. One type with two keys
  is two concepts in one home.
- Every MIR pass and the MIR evaluator would have to refuse or handle lowered statements.

**Chosen: a new IR**, `blaze-lir`, at the level above, encoded as AIR, with block parameters and
defined-by-default ops from CLIF, the facts of LLVM, and the ABI decided once as
`codegen_ssa` does. It is still one lowering of MIR: the builder moves from emitting CLIF to
emitting LIR. That keeps stencil.md's reason for taking a lowered IR ("a second lowering of MIR
would be a second copy of the IR builder") and removes the IR that forced the workarounds.

## Where the code lives

| crate | what | depends on |
|---|---|---|
| `crates/blaze-lir` (new, `#![blaze::sans_io]`) | `Func`, `Ty`, `Op`, `Op::info`, `Builder` (full and requests-only sinks, in the caller's allocator), `verify`, `print`/`parse`, `persist` (records); later `pass/` and `legalize` | blaze-format, blaze-hash, blaze-ids |
| `crates/hir-ty-codegen` | the new builder in `lir.rs` and `lir/` (MIR to LIR), beside the old one in mod.rs until the switch; the v2 adapter; the unwind and debug-info writers | blaze-lir, hir-ty, ... |
| `crates/blaze-stencil` | the patcher and glue over LIR | blaze-lir; blaze-llvm at build time |
| `crates/blaze-llvm` (new) | LIR to LLVM IR, release, PGO, sanitizers, reports | blaze-lir, inkwell, llvm-sys |
| `crates/blaze-lir-wasm` (new) | LIR to wasm | blaze-lir, wasm-encoder |
| `crates/blaze-codegen` | the driver, `Prepare`, keys, templates; `LirSpec`/`EmitSpec`, `Backend::Wasm` | unchanged otherwise |

`blaze_codegen::backend::Prepare` keeps its methods. `prepare` returns the LIR's shape and a
`Prepared` that is one struct holding the LIR, since all backends read one LIR:
`v2::Prepared::{Cranelift, Llvm, Stencil}` collapses. `masked` and `merge` work on LIR once `fold` lands after the switch.

## The way there

**Straight to LIR, no bridge.** Nothing prints LIR back to CLIF, and no consumer reads both. The
new pipeline is written beside the old one:

- MIR to LIR, porting hir-ty-codegen's lowering;
- LIR to LLVM, LIR to stencils and LIR to wasm, each built directly.

The old Cranelift path keeps building blaze and every project until the new pipeline builds
blaze and the real-project harness passes on it. Then one switch makes LIR the only path. The
CLIF lowering, native Cranelift and the duplicate codegen front end are deleted in that change
or the one right after it.

**Until the switch** the pipeline is chosen per build by a toolchain setting, `Toolchain::codegen`
(`Clif` or `Lir`, hashed into `Settings` as `llvm` and the stencil library id are). It is set
by `-Z lir` for blaze's own tests and the harness runs, and it is never chosen by a failure: a
LIR build that cannot compile a function stops with that function's typed error, and the old
path is not tried. The setting is deleted at the switch.

**Proof**, for every lane and the switch alike, is the programs blaze builds:

- `harness/v2/realprojects.py` over corpus.json's workspaces (ripgrep, nushell, bevy, polars and
  the rest), built and tested by blaze, debug and release;
- blaze building blaze;
- blaze's own tests of each crate.

Speed is read from profile spans on the exact tree (rule `measure-the-exact-tree`):

- **Dev:** `translate` (the builder), `verify ir`, and a `stencil` span `lir-stencil` adds per
  compile.
- **Release:** `release summarize`, `release emit`, `llvm compile`.
- **Generated code:** the projects' own benchmarks under `blaze bench`, before and after.

### Lanes written in parallel

`lir-ir` lands first; it is small. The others start from its API and are written side by side:
each owns its crate or directory, and none changes the old path.

| lane | what it writes | blaze's own tests it adds | needs |
|---|---|---|---|
| `llvm-memory` | the malloc family and `operator new`/`delete` through `Counting<Heaps>`; a `Context` per LLVM compile; `Tag::Llvm` ([LLVM's memory](#llvms-memory)). Lands on the old path at once. | the routing control test, foreign frees; a release build of nushell whose counted peaks and RSS agree within a stated margin, before and after | nothing |
| `lir-ir` | `crates/blaze-lir`: types, `Op`, `Op::info`, `Builder` with both sinks, `verify`, the text form, the record | record and text round trips (Hegel), the verifier's refusals | nothing |
| `lir-lower` | the MIR to LIR builder in `hir-ty-codegen/lir/`, ported from `FunctionCx`. It emits the facts table from the start, each fact a port of the rustc function named beside it. Split by area so subagents write it at once: ABI and signatures (`PassMode`/`fn_abi` made CLIF-free and shared by both builders); places, rvalues and memory; terminators, drops and unwinding; Rust intrinsics and atomics; generic SIMD and `target_intrinsic`; inline asm; statics, constants and vtables; debug info and variables. | the verifier on every function the builder makes for the corpus | `lir-ir` |
| `lir-llvm` | `crates/blaze-llvm`: LIR to LLVM IR; `release.rs`, `pgo.rs`, `sanitize.rs`, `report.rs` and `emitted.rs` moved in and reading LIR | per-fact tests that LLVM is given each fact; a planted-bug test per fact (the fact stated where it is false, such as `noalias` on a `&Cell` argument) that a program's own test catches (rule `test-the-cause`) | `lir-ir` |
| `lir-stencil` | blaze-stencil on LIR: scan, select, slots, glue, `Shape` over `lir::Op`; the generator through `blaze-llvm`'s per-op lowering, making a second library beside the CLIF one until the switch; target-intrinsic stencils | the library checks, per-op results against the Rust operation, the ABI cross-calls with LLVM code on each OS | `lir-ir`, `lir-llvm`'s op lowering |
| `lir-wasm` | `crates/blaze-lir-wasm`: `wasm::lower` and `wasm/v128.rs` on LIR, `Backend::Wasm` | the wasm emitter's own tests, moved | `lir-ir` |
| `lir-adapter` | the v2 adapter on LIR: `LirSpec`/`EmitSpec`, `LirKey` with `built_in`, the shape from the record's bytes, `Prepared` as one struct, the walk through the requests-only sink, `promote`/`copies`/`frame` and `legalize` on LIR, `Toolchain::codegen` | keys: `harness/keys.py`'s single-input changes miss, `v2/edits/identity.rs`'s edit fuzzer holds; determinism (two builds byte-equal) | `lir-ir`; joins the others |

### The switch

The switch (lane `lir-switch`) needs all of these, on one exact tree:

1. **blaze builds blaze** with `-Z lir`: the dev and release blaze, the wasm seed through the
   wasm emitter, and the proc macros blaze's build expands as wasm modules. That blaze passes
   blaze's tests.
2. **The real-project harness passes** on `-Z lir` for every project, debug and release, on
   macOS AArch64, Linux x86-64 and Windows x86-64 (ix VMs).
3. **No slower**: the dev build's `translate` + `stencil`, the release build's `release summarize`
   + `release emit`, and the projects' benchmarks, each against the old path on the same tree.
   A regression is fixed before the switch, not after.
4. **Keys and the warm store**: `harness/keys.py` and `harness/warm_store.py` pass. The store
   rekeys once at the switch, which is expected, and that is the release note.

The switch is one change: `Toolchain::codegen` is removed, and LIR is the pipeline. In it, or
the change right after (`lir-delete`), these are deleted, each name banned in `.clippy.toml`
with its replacement (rule `one-concept-one-home`):

- **The CLIF lowering:** `FunctionCx` and its CLIF code in mod.rs; the CLIF halves of `neon.rs`,
  `x86.rs`, `avx512.rs`, `sve.rs`, `fma.rs`, `wasm_simd.rs`, `f16_f128.rs`, `arbitrary_int.rs`,
  `copies.rs` and `promote.rs`; `overlap.rs`, `float_pair.rs`, asm.rs's stub paths,
  `neon-fma-llvm.txt` and `sve-llvm.txt`.
- **Native Cranelift:** `emit_with`'s `Context::compile`, `emit_lent`, v2's `compiled`,
  `isa_for`/`Isa`, `tier.rs`, `blaze_stencil::tier::instrument`, the CLIF stencil library and
  `src/tests/clif/`.
- **The duplicate front end:** v1's drivers (`compiled_root`, `root_code`, `instance_code`'s
  `Detach::No`, `fncache.rs`'s use of them, `backend.rs`'s `Backend` with `BLAZE_BACKEND` and
  `LLVM_LIMIT`), and v2's mirror (`v2/refs.rs`, `v2/refs/intrinsics.rs`).
- **The CLIF canonicalizers:** `v2/shape.rs` and `v2/fold.rs`'s CLIF masking.
- **CLIF to LLVM:** `llvm.rs`'s and `llvm/lower.rs`'s CLIF lowering, `llvm/rebase.rs`, `REBASED`,
  `llvm/lower/vector.rs`; and `wasm.rs`'s CLIF reader.
- **The manifests:** `cranelift-codegen`, `cranelift-frontend`, `cranelift-interpreter` and
  `cranelift-reader` leave every one but Wasmtime's. A test holds that no blaze crate's
  dependency closure has `cranelift-codegen` but through `wasmtime`.

**After the switch**, measured, each by itself (Andrew, 2026-11-09): keeping LIR in the store
for release imports and tier-up (`blaze_cas::Kind::Lir`), facts beyond rustc's set, and
folding near-equal functions on LIR.

**The rule and doc changes with the switch.**

- stencil.md's "The input: Cranelift IR after ABI lowering" becomes LIR, and its tier-up
  sentence (stencil.md:12, "Hot functions are recompiled with Cranelift or LLVM") becomes "with
  LLVM".
- compiler-correctness's note "Cranelift's IR types may stay as the stencils' input until blaze
  has its own IR" is dropped from `tools/agents/rules.ts`, which generates AGENTS.md and the
  skill.
- plan.rs's comment that Cranelift optimizes wasm32 code (`opt_level=speed`) is corrected:
  `speed` never reached wasm32's emitter.

## Risks, ranked

1. **A fact that is false is a silent miscompile.** A `noalias`, `range`, `inbounds` or `nsw`
   claimed where Rust does not guarantee it changes a correct program's behaviour only under
   optimization, and only sometimes. Since there is no bridge, the facts arrive with the new
   path, not one at a time on the old one. *Settled by:* every fact's rule being a port of one
   named rustc function for the pinned nightly; a planted-bug test per fact; real projects'
   release test suites at the switch. A release failure is bisected by masking facts in
   `blaze-llvm` (a test-only setting, never a fallback).
2. **The stencil and LLVM ABIs split** once Cranelift's `abi.rs` stops being the reference.
   Their code calls each other at tier-up and in units of a CPU without the stencils'
   baseline, so a mismatch corrupts values silently. The Windows float pair already differs
   (`float_pair.rs` on the stencil path only). *Settled by:* `lir-stencil`'s Hegel signature
   generator, with cross-calls both ways on each architecture and OS, before the switch.
3. **One big switch.** With no bridge, the new builder, three consumers and the adapter meet
   only when `-Z lir` builds real code, and a bug may be in any of them. *Settled by:* `-Z lir`
   working early on small programs (the corpus) and growing to blaze and the real projects
   while the old path still builds everything; by the verifier on every LIR; and by each
   consumer's own tests against hand-written LIR text before the builder is done.
4. **The builder port is large**, about 17,000 lines in mod.rs and ten sibling files, written
   beside the old builder, which lands fixes meanwhile. *Settled by:* splitting it by area
   across subagents, keeping the CLIF-free parts (layouts, `PassMode`, names, statics) shared so
   a fix there serves both, and a weekly list of the old builder's changes not yet in the new
   one, which must be empty at the switch.
5. **The stencil library grows** with target intrinsics, 256- and 512-bit operands and scalable
   registers: `core::arch` declares thousands of `link_name`s per architecture. *Settled by:*
   counting them in the pinned library, generating the ones real projects reach, and measuring
   the library's bytes and build-script time. An intrinsic with no stencil is a typed error
   naming it (no fallback).
6. **Malloc routing in the process** (`llvm-memory`). It touches every C allocation, and a block
   another image made and blaze freed into mimalloc corrupts the heap. *Settled by:* the
   foreign-free check and its tests on macOS and Linux, and a release build of nushell and the
   tests job (memory-budget.md's case) with the routing on.
7. **A context per LLVM compile costs its creation** and the type tables LLVM made once per
   thread before. *Settled by:* the `llvm compile` span before and after on nushell's release
   build. A context of a few hundred bytes made per function that compiles in milliseconds
   should not show.
8. **Windows unwinding** moves from the builder into the stencil glue (the guarded call).
   *Settled by:* `harness/unwinds.py` and `harness/unwindsplice.py` on a Windows ix VM, mixing
   stencil and LLVM frames in one unwind.

Settled by Andrew (2026-11-09), once open questions:

1. **TBAA: none.** Rust has no type-based aliasing and rustc emits no `!tbaa`; LLVM gets
   rustc's facts (alignment, dereferenceable, ranges, `noalias` from references) and no more.
2. **Facts beyond rustc's set** (alias scopes for bodies blaze's MIR inliner builds in, ranges on
   loads rustc leaves plain) come after the switch, each its own measured change.
3. **No LIR is kept at the switch.** Keeping it for release units and tier-up comes after, once
   measured.
4. **Folding** near-equal functions on LIR comes after the switch, with whether it runs for
   every backend.

The record and encoding rules above are [data.md](data.md)'s, [memory.md](memory.md)'s, the
memory-and-data skill's and blaze-format's (`record!`, "the bytes in memory are the bytes on
disk").

## Related

- [code-identity.md](code-identity.md): the compile identity the `LirKey` starts from.
- [caching.md](caching.md): keys of content only.
- [icf.md](icf.md): folding before and after codegen.
- [tail-calls.md](tail-calls.md): `become`, which `tail_call` carries.
