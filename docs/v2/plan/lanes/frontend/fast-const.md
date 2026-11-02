---
status: plan
summary: frontend-fast-const — constants of the closed shape (integer, bool and char literals, paths to other constants, checked arithmetic, integer casts, size_of/align_of, array lengths) evaluated over HIR with the MIR evaluator's exact bytes and errors; every other constant by MIR evaluation; dependencies' constants served by content from the global cache.
date: 2026-10-29
---
# frontend-fast-const: constants over HIR, and dependencies' constants by content

Section 6 of [fast.md](fast.md). Written 2026-10-29; nothing was built or run, so every cost
below is counted from the code, and every claim of agreement is what a named test checks once
the tree builds.

## 1. The work and its lower bound

Machine figures: the top of [../db/speed-of-light.md](../db/speed-of-light.md) (3.5 GHz, L1 4 to
5 cycles, L2 14, L3 50, DRAM 110 ns, branchy code at 2 IPC).

Most constants in real code are a literal (`const MAX: usize = 4096;`), a path to another
constant, a few operators over those (`(1 << 20) - 1`, `PAGE * 4`), a cast, `size_of::<T>()`,
or an array length (`[u8; N * 2]`). The work for such a constant of `n` nodes:

- read each node: its kind and children (one line of the store's columns), its type (one line
  of inference's type column) and whether it has adjustments (one probe of a CSR offset row):
  3 lines, all L1 or L2 after the first, about 15 cycles a node;
- apply the operator with rustc's check: 2 to 5 instructions, no branch mispredicted in the
  common case;
- read each named constant: one salsa memo hit, about 5.5 ns
  ([../db/speed-of-light.md](../db/speed-of-light.md), "Memo hit");
- intern the result: hash at most 16 bytes and probe the allocation table, about 25 ns, plus
  the result's boxed bytes (one small allocation, about 20 ns).

Bound: about `5 ns × n + 5.5 ns × consts + 45 ns`. A literal constant: about **50 ns**, almost
all of it the result's interning. A constant already evaluated in this revision: one memo hit,
**5.5 ns**. A dependency's constant evaluated in an earlier session: one store probe of a
memory-mapped table, about one DRAM miss and a key compare, **≈ 150 ns**, with no parse,
lowering or inference of its body.

## 2. What the code does today (counted from the code)

Every constant goes `const_eval_query` → `monomorphized_runtime_mir_body` → `interpret_mir`
(`crates/hir-ty/src/consteval.rs`):

1. MIR lowering (`hir-ty-mir/src/mir/lower.rs`, `lower_to_mir_with_store`): a `MirLowerCtx`
   (a dozen maps and vectors), a local per temporary, a basic block per check; `const X: u8 =
   A + 1;` lowers to 2 temporaries and 1 `CheckedBinaryOp`.
2. The runtime pipeline (`transform.rs`): the asserts pass (`transform/asserts.rs`) turns each
   checked operation into `AddWithOverflow` + `Assert` + a new block, then drop elaboration and
   cleanup; the result is memoized as a `MirBody`.
3. Monomorphization with empty arguments (a fold over the body), memoized again.
4. `Evaluator::with_machine` (`hir-ty-mir/src/mir/eval.rs`): builds an inference context
   (`infer_ctxt().build`), the target layout, a stack and heap `Vec`, a `BTreeMap` of heap
   blocks, `RefCell`ed provenance, the lang items; then `interpret_mir` creates the locals,
   runs every block (each statement through `eval_rvalue` with `SmallVec` operands), and
   `create_memory_map` walks the result type.

For a literal constant that is 3 salsa queries, at least 20 heap allocations, and about 5 to
20 µs: **100× to 400× off** the bound. For a dependency, today's code also parses, lowers and
infers the body first.

## 3. The design

### 3.1 The shape, classified once

`consteval/hir_eval.rs`, `Program::of`: one walk of the body's expression tree that either
compiles it to a flat postorder program of at most 256 ops (on the stack, `ArrayVec`) or says
the constant is outside the shape. The four evaluation queries call `hir_eval::value`, which is
`by_hir` for a constant of the shape and `by_mir` (the queries' MIR code, moved) for any other;
`both` runs the two for the differential tests. A constant is in the shape when all of these hold, and is
evaluated by MIR otherwise; that is the one path for it, so this is a split by shape, never a
fallback (a constant in the shape never reaches MIR evaluation, and one outside never reaches
this evaluator):

- **Owner.** A `const` item (`const_eval_query`), an anonymous constant that is not an inline
  `const { .. }` (`anon_const_eval_query`: array lengths and repeat counts), or an enum
  variant's explicit discriminant. Statics are not in it (their value is a place).
- **The body.** Inference of it has no type mismatch, no diagnostic and no error; its store has
  no macro call (MIR lowering refuses a body with a failed expansion, `check_expansions`, and
  the store cannot tell a failed one from a good one without the source map); its tree has at
  most 256 nodes; the const-eval budget and target of the constant's own crate (the ones
  `Evaluator::with_machine` reads, not the environment's) have at least 4096 steps and 1 MiB
  (a tree of 256 nodes runs at most 3 blocks a node, so MIR evaluation of it can never run out)
  and a data layout. A constant of the shape uses no generic parameter, so its value is the
  same for every `subst`; layouts are asked in the evaluation's environment, as the
  evaluator's `layout` asks them.
- **Every node** has no adjustment, and a type of kind `Int`, `Uint`, `Bool` or `Char`, and is
  one of:
  - a literal: `Int`, `Uint` (truncated to the type's width, as `lower_literal_to_operand`),
    `Bool`, `Char`; `-lit` of an `Int` literal is the negated literal, as lowering has it;
  - a path whose inference resolution (`InferenceResult::value_resolution`) is a `const` item
    of a module (no associated resolution, no generics), read through `const_eval_query` with
    the evaluator's environment, exactly as `Evaluator::allocate_const_in_heap` reads it;
  - `-e` on a signed integer, `!e` on an integer or `bool`;
  - `a op b` where both sides are `Int`/`Uint`/`Bool`/`Char`, the same type except for shifts
    (`lower.rs` `is_builtin`), and `op` is arithmetic or bitwise on integers, bitwise on `bool`,
    a comparison, or `&&`/`||` on `bool`;
  - `e as T` with `e` and `T` both integer-like (`Int`, `Uint`, `Bool`, `Char` to `Int`,
    `Uint`; `u8` to `char`): `IntToInt`, or a coercion cast, which keeps the bytes;
  - a call with no arguments of `core::intrinsics::size_of`/`align_of` (by `intrinsic_of`), or
    of `core::mem::size_of`/`align_of` (their diagnostic items, in a crate of the toolchain),
    whose one type argument has no parameter, inference variable or error, and whose layout
    is `Ok` and sized, and whose type is `usize`; the value is `layout.size` or `layout.align`,
    as the shim's;
  - a block with no statement and no label whose tail is in the shape.

### 3.2 Evaluation, with rustc's checks and MIR's exact errors

`consteval/hir_eval/ops.rs`: values are `u128` bits with a 1-byte kind (width, signedness);
each operation is Rust's own operation on that width. The checks and their order are those the
asserts pass gives a constant (`transform/asserts.rs`, with `checks_overflow` true for every
non-function owner):

| operation | check, in order | panic message |
|---|---|---|
| `+ - *` on integers | overflow | `Overflow in Add` / `Sub` / `Mul` |
| `/ %` | divisor zero, then (signed) `MIN / -1` | `Overflow in Div` / `Rem` (both) |
| `<< >>` | right side, as unsigned of its width, `< bits` | `Overflow in Shl` / `Shr` |
| unary `-` (signed) | operand `== MIN` | `Overflow in Neg` |

Operands are evaluated left before right, each fully, before the operation's checks: lowering
puts every non-literal operand in a temporary first (`lower_expr_to_some_operand`), so a
failing constant on the left wins over one on the right and over the operation's own panic.
`&&`/`||` skip their right side as MIR's `SwitchInt` does.

The errors are the values MIR evaluation gives, built the same way:

- a failed check: `MirEvalError::InFunction(Box::new(Panic(message)), vec![])`: the `Assert`'s
  panic unwinds out of the constant's frame, whose owner is no function, so the trace is empty
  (`unwind.rs`, `raise` and `unwind_out`);
- a named constant that fails: `InFunction(Box::new(ConstEvalError(name, Box::new(e))), vec![])`
  with `name` from `GeneralConstId::name`, as `allocate_const_in_heap` wraps it and
  `interpret_mir` adds the empty frame list.

The result is `AllocationData { ty, memory, memory_map: MemoryMap::Empty }`, `ty` the root's
type (MIR's return slot: `expr_ty_after_adjustments(root)`, with no adjustment) and `memory`
the value's low bytes in little-endian: what `Evaluator::evaluate` gives for a scalar. Since
allocations are interned, the same bytes give the same `Allocation` and so the same `Const`.

E0080 and the lints: blaze reports a failed constant from its `ConstEvalError` value, and the
`arithmetic_overflow`/`unconditional_panic` lints (`blaze-diagnostics`, `lints/bodies/panics.rs`)
read constants through `const_eval`; both see identical values from either path, so codes,
primary spans and lint levels are unchanged by construction.

### 3.3 Dependencies' constants by content

`consteval/deps.rs` (child `frontend-fast-const-deps`, notes
[fast-const-deps.md](fast-const-deps.md)): a dependency's `const` item with no generic
arguments, once evaluated, is kept in the store and the global cache under a key of the crate's
check-result key (frontend-fast-bodies-cache, built on frontend-deps' image key: the crate's
content, its dependencies, the toolchain, the target and `build`) and the item's stable key
(frontend-deps' signature item key). `const_eval_query` asks it first for such a constant; a
hit is one probe and decodes the bytes, a miss evaluates (by shape, as above) and records the
value for the driver to write. The value is in stable form (types with no salsa id); a value
that does not read back under its key is an error naming the constant, never a re-evaluation.

### 3.4 Prior art, and what this matches

- Clang's `ExprConstant.cpp` fast path for integer constant expressions (`EvaluateAsInt`)
  beside its full constant interpreter: the same split by shape.
- Zig's comptime evaluating directly on its typed IR (Sema), not on lowered machine IR.
- rustc's `Const::try_from_lit` / `lit_to_const` for literals in types; this extends it to the
  arithmetic around them, with const-eval's exact errors.
- rustc's cross-crate const values in `.rmeta` (`eval_to_const_value_raw` results are encoded
  for every `const` item): the dependency cache is that, shared across machines.

## 4. What proves it

- `crates/hir-ty/src/consteval/hir_eval/tests.rs` (no database):
  - `ops_match_the_reference`: a reference written as the MIR evaluator computes (each value
    its native integer type read from its little-endian bytes, `checked_*` for the asserts
    pass's checks, the shift amount as the unsigned type of its own width, `as` for casts)
    against `ops.rs` on every operation, every integer type, `bool` and `char`, at the edges
    (0, 1, -1, MIN, MAX, MIN+1, the width and its neighbours) and on random operands (2,000
    rounds of every kind and operator): identical bits or identical panic.
  - `every_check_fails_where_rustc_s_does`: each row of the table above at its boundary, for
    every integer type.
  - `generated_trees_match`: random expression trees (literals of every integer type, every
    operator, casts, nested constants that fail) run by a reference tree-walker that follows
    MIR's statement order against the compiled program: identical bytes or error.
  - `programs_allocate_nothing` (a literal's program and a failing one, with a control that
    the counting allocator is installed) and `sizes` (`Op` at most 32 bytes, `Kind` and `Panic`
    one).
- `crates/hir-ide/src/tests/consteval/hir_eval.rs` (with a database, both evaluators; hir-ide is
  where the consteval fixtures are): every fixture `GOAL` of `tests/consteval.rs` and its
  submodules, and every other constant and explicit discriminant of its crate, is run by both
  paths when it is in the shape (`eval_goal` calls `check_all`; a coordinator request),
  hand fixtures pin the shape's edges (`the_shape`, `the_checks`, `layout_facts`,
  `outside_the_shape`, each with the count of constants it must classify in the shape), and
  generated programs (30 files of 40 constants, each also required to be in the shape) (random literals and operators of every
  integer type, overflow, division by zero, shifts out of range, failing named constants) are
  written as fixtures and evaluated by both: identical `Allocation` or identical
  `ConstEvalError`. Allocation budget (`blaze-alloc-count`): a second `const_eval` of a
  constant (a memo hit) allocates nothing (`a_memo_hit_allocates_nothing`); evaluating a
  program allocates nothing (hir-ty's `programs_allocate_nothing`), so a literal constant's one
  allocation is its bytes for the interning.
- `harness/v2/frontend/cases/const/` (child `frontend-fast-const-cases`): at least 40 programs
  run through rustc and blaze by `frontend-fast-diff`'s harness, covering each shape above and
  each edge where a constant leaves it (a method call, a float, a static, a generic, a macro),
  with the expected E0080s and lint outcomes.
