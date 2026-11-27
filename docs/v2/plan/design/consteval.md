---
status: plan
summary: The rebuilt constant evaluator, crates/blaze-consteval. It interprets the checker's typed body form with an explicit work stack, step, memory and call-depth bounds, and memory with provenance. A value leaves as a record of bytes plus relocations, keyed from text. The seams are `eval_len` and `eval_discriminant` for the checker and `value_of` for the builder.
date: 2026-11-16
---
# The constant evaluator

`crates/blaze-consteval` computes what Rust needs at compile time:
- `const` items;
- `static` initialisers;
- array lengths and const generic arguments;
- enum discriminants;
- inline `const` blocks;
- the `const fn` calls inside all of these.

The tests are in `src/tests.rs`. Each case is Rust text run through the driver's own pipeline (blaze-lower, `to_typeck`, blaze-typeck's `check`), and each names its rule from the Reference.

## What is interpreted

The evaluator walks the typed body form (`BodyView` plus `Checked`), not LIR, for four reasons:

1. A constant's value is needed before code exists: an array length is part of a type, so the checker needs it before any LIR is built.
2. The typed form already has every resolution the evaluator needs, as sorted columns: callees, adjustments, field rows, and path definitions.
3. Literal values are read from the item text at their item-relative ranges, so the evaluator needs nothing else.
4. Interpreting the typed form keeps LIR out of the checker's dependencies. If LIR later has the same semantics and is cheaper to reach, the machine can take a second input behind one seam (rule 78).

## The model

- **Values.**
  - Integers of every width, held in a `u128` with their type's width. Overflow is an error (`Overflow{op}`), and so are division or remainder by zero, `MIN / -1`, `MIN % -1`, and a shift by the width or more or by a negative amount.
  - The wrapping, overflowing, saturating, rotating and bit-counting operations are intrinsics.
  - `bool` and `char`; a read checks validity (`InvalidBool`, `InvalidChar`).
  - `f32` and `f64` support `+ - * / %`, comparisons, negation, and casts. They follow IEEE 754 with round-to-nearest, as at run time; float-to-int casts saturate and NaN becomes 0. `f16` and `f128` are refused by name.
  - Tuples, structs, unions, enums, arrays, slices and `str`.
- **Memory.** Memory is a list of allocations, each with bytes, an init bitmask, relocations, an alignment, a mutability flag, a liveness flag and an origin (data, local, static, fn).
  - A pointer is an (allocation, offset) pair, never a number. A fn pointer points at the allocation of its fn instance, keyed by `InstanceKey`.
  - Reading uninitialised bytes gives `Uninit`; a read past the end gives `OutOfBounds`; a dead allocation gives `Dangling`.
  - Using a pointer's bytes as an integer gives `PointerAsInt`, and a pointer to an integer address gives `IntAsPointer`. Reading part of a pointer gives `PartialPointer`; comparing pointers gives `PointerCompare`.
  - There is no `unsafe` beyond `transmute` of same-sized plain data (`TransmuteSize`). The crate itself is `forbid(unsafe_code)`.
- **The machine.**
  - One loop pops tasks: evaluate an expression as a value or as a place, apply its adjustments, or continue an operator whose operands are done. Results go on one stack. `break`, `continue` and `return` unwind to their marker task. Frames hold locals by `PatId`, and each frame caches its grounded types. Nothing recurses on the input; a 100,000-deep expression is 100,000 tasks.
  - Pattern matching uses a work list with or-pattern choice points and default binding modes.
  - Temporaries of an expression statement end with that statement; a frame's locals end when the frame returns. A reference that outlives either is `Dangling`.
  - Drops are not run: a type with drop glue in a constant is unsupported, and drop glue is not built yet.

## The limits

`Limits::DEFAULT` sets three bounds; passing any of them is a typed refusal that names the constant and the limit (rule 18):
- 20 million steps (`StepLimit`);
- 64 MiB of live memory (`MemoryLimit`);
- 512 nested calls (`CallDepth`).

`Settle` bounds the nesting of needs in the same way.

## The record

`ConstRecord { key, data, reads }`:
- **`data`** is a `DataRecord`: a list of allocations, each with its bytes (padding and unwritten bytes are 0), `align_log2`, `mutable` and its relocations. A relocation is `{offset, target}`, where the target is `Local(i)`, `Static(item)` or `Fn(InstanceKey)`. Its eight-byte slot holds the addend.
  - The root is allocation 0. The other allocations are in the order the root reaches them, so the record holds no address and no map order (rule 25).
  - The form is blaze-build's (`blaze_build::data::DataRecord`): record.rs re-exports it, and every record passes its `check` before it leaves.
- **`key`** is `RecordKey::of(instance_key(code_id(item), ground args))`, computed from text before the work (rule 65).
- **`reads`** lists what the evaluation read, sorted and deduplicated: each body's text hash and each other value's key.

An evaluation that needs another value's record, or an unchecked body, returns `Answer::Need`. The driver gets what is missing and re-enters from the top. `Settle` is a sans-IO stack of needs in progress, and a need already on it is reported as `Cycle{chain}`, naming every constant on the cycle.

## The seams

Each caller makes one call:
- **`value_of(world, layout, item, args, ground, limits, alloc)`**: the builder's seam, used for consts, statics and inline consts. It returns the record or a need.
- **`eval_len(...)`**: the checker's seam for an array length or a const argument of type `usize`. It returns the `u64` and the record to keep; `len_of(record)` answers `World::const_value` later.
- **`eval_discriminant(world, adt, variant, alloc)`**: the checker's seam for discriminants. It walks back to the nearest explicit discriminant (whose record it reads) and adds the distance, in the enum's `repr` type (`isize` when there is none). Passing the type's range gives `DiscriminantOverflow`.

`ConstWorld` is what the driver implements over the item layer and the checker's records: bodies, fn kinds (const, not const, ctor, intrinsic, panic entry), impl resolution, code ids, records, and explicit discriminants. `Layout` answers the sizes, offsets and tags. blaze-build's `Layouts` implements it (in layout.rs), so compile time and run time agree; the tests and the cost example use it too. One `Layouts` lives as long as its unit and target, beside the ground table its ids are of, and every evaluation of the unit takes the same pair. An enum's explicit discriminants are values the layout reads from their records: while one is not made, the layout refuses with `Missing` and caches nothing, and the evaluation returns that discriminant as a need. Nothing in the machine checks it again.

## What is refused, and why

What the language refuses in a constant is refused under its own name:
- the arithmetic errors listed under *Values*;
- a non-`const` fn (`NotConstFn`);
- reading a `static mut` (`MutableStatic`);
- the memory errors listed under *Memory*;
- a panic (`Panic{parts}`, the message's arguments);
- `unreachable` and a false `assume` (`Unreachable`).

`Unsupported{what}` names what is not built yet:
- overloaded operators, `Deref` and `Index` (const traits);
- closures;
- calls through fn pointers;
- unsizing other than array to slice;
- constant patterns of aggregate types;
- intrinsics outside the list in `Intrinsic`.

**Pointers.** The Reference allows comparison operators in constants only on integers, floats, `bool` and `char`, and excludes pointer-to-address casts. So `==` and `<` on pointers are refused (`PointerCompare`), and so are pointer-to-integer casts (`PointerAsInt`).

`ptr_guaranteed_cmp`, behind `guaranteed_eq`, `guaranteed_ne` and core's `is_null`, must never be wrong; it may answer "unknown" (2). It answers:
- 1 or 0 for two pointers into one allocation, by their offsets;
- 0 for null against a pointer into, or one past the end of, an allocation;
- 0 for pointers strictly inside two allocations when at least one has an address of its own: a live local of the evaluation, or a static;
- 2 otherwise. Temporaries and other constants' data may be merged with equal data in the program, a fn may be merged with an equal one, and one past the end of one allocation may be the start of another.

The test `pointers_compare_only_when_certain` states each case, including `is_null` of a pointer made from a reference, written as core writes it.

**A constant used is a fresh value.** The Reference ("Constant items") inlines a constant wherever it is used, so each use is its own temporary. A read takes the decoded record in place; a borrow (`&K`, an autoref, a `ref` binding) copies the record's root into a temporary of its own. So `&K[0]` twice is two allocations (`ptr_guaranteed_cmp` answers 2), and a write through `&mut K` reaches only its copy.

**Literals are read where they were written.** A body's ranges are item-relative sites (rule 71), and a literal made by a macro expansion has the call as its site. So `Typed` carries `Lits`: each literal expression's and literal pattern's written text, which the driver reads from the lowering's view (`View::text_at`). The machine reads a literal's value only there.

**Layout is the builder's.** `emitted_code_reads_constants_back` places three records in memory, and code blaze-build emits (`read_back`) loads every scalar leaf through the builder's offsets. The three are a reordered struct with a `&str`, `[E; 4]` of a `repr(u8)` enum, and `&(u16, &[u8])`. Each leaf equals what the source wrote.

**Bounds.** `hostile_constants_stop_by_their_bound`: unbounded recursion stops at `CallDepth`, an endless loop at `StepLimit`, and `[0u8; 1 << 30]` at `MemoryLimit` before any store is counted. A constant that reads itself through a reference stops as `Cycle [SELF_REF, SELF_REF]`. Each names the constant.



**Panics.** The Reference makes a panic where the value must be known at compile time an error. A constant reaches core's const panic entries: `panicking::panic(&str)`, behind `panic!("lit")`, `assert!`, `unreachable!` and their kin, and `panic_display`, behind `panic!("{}", s)` of a `&str`. The finding is a `ConstError` that carries:
- the message's parts;
- the constant asked for;
- the panicking call's item-relative range and item, which may be a `const fn`'s (rule 71).

## Cost

The floor of a constant is the sum of its record key (two hashes, about 160 ns) and 5 ns per
step (a register interpreter's dispatch); an evaluation runs in a task's `blaze_mem::Frame`,
rolled back after it. On that floor `examples/cost.rs` puts every row, from `1 << 20` to a
256-entry CRC table, at 1.4x to 2.2x. Left, each from the profile:

1. Shrink `value_of`'s frame (over 4 KB): move `evaluate` and `intern` out of line and keep
   `Work` borrowed, not moved. The key (`RecordKey::of`, `instance_key`) is more than half of
   the smallest constant's time.
2. Make `Op` 16 to 32 bytes, with immediates in a side table: the 80-byte `Op` is copied per
   step, about 12 ns a step against the 5 ns floor.
3. Let `step` return a small `Flow`, with the error parked on the machine.
4. Let a register hold a constant's scalar value, so a scalar `Const` needs no decode.

## Comptime on the item layer (design; rule 29)

Comptime and reflection are the one primitive that other features are libraries over (extensions.md section 4 "B"). `mark`, `bits`, `canonical_cbor`, `policy`, `memo`/`cache`, `holds` and `program_lifetime` are read or derived through them. comptime.md and reflect.md hold, except where a decision below says why another form is better.

1. **A body is compiled once into code, and that code is a record.** The code is a flat op list over numbered slots, made from a body at one ground instance. At compile time:
   - every type is grounded;
   - every adjustment is folded into the ops;
   - every literal is parsed;
   - every field offset, tag write and tag read is resolved through the unit's `Layouts`;
   - every callee is resolved to an instance.

   A local whose address is never taken is a register: a `u128` with an optional pointer. One whose address is taken is an allocation, as today. The record's key is the body's digest, the instance's arguments by digest and the target. Its read list holds the layout and type records it consulted. Every evaluation on every machine shares it (rule 52). An evaluation is that code plus its arguments, run by one loop over the ops. A call pushes a frame of slots, with no recursion in the host.
2. **A type is a value by its digest.** A `TypeId` constant's bytes are the content digest of the ground type's structural key. A constant holding ids is then plain bytes, equal in every evaluation and on every machine. This replaces comptime.md's per-evaluation id list and its remapping (`own_type_id`), which a record shared across machines cannot carry. `name_of(id)` and `type_of!(id)` look the digest up in the unit's type records. A digest with no record is a `Need`, then a typed refusal.
3. **Reflection queries are records with read lists.** Each query is answered from the signature record of the item it names, keyed by that item's own signature digest, never the crate's:
   - `fields_of::<T>`, `variants_of::<T>`, a field's `INDEX`, `NAME` and `MARKS`;
   - a variant's `DISCRIMINANT`;
   - `fn_info::<F>` (its `code_id` is the body's digest);
   - `size_of`, `align_of` and offsets (layout records).

   The digest of every record a query read goes into the evaluation's read list. An edit to a struct's fields changes its signature digest, so exactly the evaluations that reflected on it fail their read-list check and run again. An edit to a body changes only `fn_info`'s `code_id` readers.
4. **`mark` is the one attribute primitive.** `#[blaze::mark(M)]` names a type `M`, resolved to its item, and is kept in the marked item's signature record. `Marked<M>` and `.marked::<M>()` are answered from it. `#[blaze::attr("..")]` folds into marks: a string is unchecked and a type is resolved (rules 30 and 76). `policy`, `bits`, `canonical_cbor` and `invariant` become marks read by libraries.
5. **Comptime is a task.** Its key is the code identity of what it runs, the arguments by digest and the target. Its result is a `ConstRecord`, and its read list is (3)'s. A record is valid by its key, or else by its read list checked against today's facts. Nothing invalidates by hand.
6. **`for const` and `x.[f]`.** The checker checks the body once, generically, against the field-binder bounds. Code for an instance unrolls the loop at compile, once per element of the domain query's answer, in declaration order. `x.[f]` becomes the field's resolved offset. The domain rule (which fields, the filters) is one function that the checker, the code compiler and the builder all call.
7. **`where const { COND }`.** The entry is `value_of` on the condition's anonymous constant at the instance's ground arguments, read as a `bool`. A panic is "does not hold", and its message is the reason shown. A `Need` is the goal's need. The answer is a record like any constant's: keyed by the condition's code identity and arguments, with a read list.
8. **`const static { .. }`** is an evaluation of its own with comptime rights. It may call non-const fns, `dyn` and fn pointers, and allocate on its const heap; heap the value reaches through shared, `Freeze` data becomes the record's own allocations. It refuses foreign calls (`Foreign`) and pointer addresses (`PointerAddress`) by name, as comptime.md says.
9. **`type_of!(desc)`.** A `TypeDesc` evaluated is a structural type, keyed by the digest of the evaluated description. Two equal descriptions are one type in every crate. A description that depends on itself is a typed `Cycle` naming the alias and the constant.
10. **Order:**
    1. type values and the reflection queries;
    2. `for const` and `x.[f]`;
    3. `where const`: until (1) it runs on what is already there, with `value_of`;
    4. `type_of!` and built structs.

    The probe is `crates/blaze-real/extensions` (`b_comptime_*`, `b_reflect`, `b_type_of`, `b_where_const*`). The real-code check is `library/core/src/reflect.rs`, evaluated.

## Not built

- associated consts with generic parameters: `Def::Const` carries no arguments;
- drop glue;
- `const` trait impls;
- heap allocation in constants (`const_allocate`);
- pattern types: `Duration`'s `Nanoseconds` and `NonZero`'s inner types are
  `pattern_type!(..)`, which `PNode` lacks, so constants over them (`Duration::MAX`,
  `NonZero::<usize>::MIN`) cannot be checked; the evaluator needs the base type's layout plus its
  valid range, and `transmute` by bytes, which it has;
- comptime and reflection (extensions.md section 4 "B"): the next piece.
