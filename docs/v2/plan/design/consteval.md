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
- inline `const` blocks (once the body form has a node for them);
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
- `f16` and `f128`;
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

## Cost (examples/cost.rs, 2026-11-16, Mac, release)

The floor of a constant is the sum of two costs, because an evaluation pays both, one after the other. One is its record key: two hashes, measured at 160 ns. The other is 5 ns per step, a register interpreter's dispatch. The tables before 2026-11-16's sum floor (both above, and the enum-facts column) used the larger of the two. A step is now one operation of compiled code; before the op list it was one work-stack task, about one expression, so the step counts fell (POW 318 to 158). Each row is one run through `mixcheck.sh blaze-consteval run cost` on a shared Mac. "Tree walk" is the last measurement of the old machine; "op list" is the new one, each evaluation in a task's `blaze_mem::Frame` rolled back after it, as the builder runs one (rule 11).

| constant | tree walk steps | ns | ratio | op list steps | ns | ratio | enum facts ns | ratio |
|---|---|---|---|---|---|---|---|---|
| MAX `1 << 20` | 8 | 798 | 5.0x | 4 | 314 | 2.0x | 301 | 1.9x |
| MASK | | 895 | 5.7x | 5 | 326 | 2.0x | 315 | 2.0x |
| LEN `4 * size_of::<u64>() + 1` | | 970 | 6.1x | 6 | 352 | 2.2x | 336 | 2.1x |
| DEFAULT (struct with `&str`) | 12 | 1837 | 11.6x | 6 | 419 | 2.6x | 411 | 2.6x |
| POW (loop in a const fn) | 318 | 6840 | 4.3x | 158 | 1465 | 1.9x | 1331 | 1.7x |
| LVL (enum match, const fn, `as`) | 33 | 1956 | 11.9x | 19 | 551 | 3.4x | 511 | 3.2x |
| CRC (256-entry table) | 151335 | 2.93 ms | 3.9x | 69910 | 0.53 ms | 1.5x | 0.46 ms | 1.3x |

Against the old step floor (5 ns per tree-walk step), POW's 1465 ns is 0.9x of 1590 ns and CRC's 0.53 ms is 0.7x of 0.76 ms.

What changed:
- **The op list** (`machine/code.rs`). A body is compiled once per (item, ground arguments) into `Code`: ops over numbered registers, a type table of `TyInfo`, parsed literals, field offsets, tag writes, constant record keys, callees and enum discriminants. A run reads none of the body form. `Codes` holds a unit's code and lives as long as its ground table and layouts. A local whose address is never taken is a register; a taken one is a memory slot, found by compiling again with that pattern marked. What compiling meets that is refused, or a record not made yet, becomes an `Op::Late` that stops the run only when reached. Code with a `Need` in it is compiled again when next asked for. The compiler is an explicit work stack; the run is one loop over frames, with no recursion.
- **Memory is one heap** (`mem.rs`). Every allocation is a region of one byte heap with one written-bit per byte. Regions are given back in stack order, and an id is never reused, so a pointer to an ended one stays `Dangling`. Before, each allocation held three vectors and `copy` made three more.
- **Discriminants are found when compiling** (`Op::Discr`). The explicit discriminants' record keys go in the code's read list and into every run's reads. Before, each `as` hashed the discriminant's record key again.
- **The root is interned in place** when the value is a whole temporary of its size.
- **Enum facts, checked once** ("enum facts" column; `code::EnumFacts`). The first time a code reads an enum, compiling finds where its tag is and how the tag decodes (`layout::Tag`, from the layout) and all its discriminants in one forward pass, each checked to fit the enum's integer (`discriminants_planted`). Only `Compiler::enum_facts` makes an `EnumFacts`, so an op holding its index needs no check: `match` (`Op::IfVariant`), `as` and `discriminant_value` (both `Op::Discr`) decode with no `Layouts` lookup. Before, each arm made two lookups and `discriminant_value` walked the discriminants back at run time. The same change inlined `malformed`, which `ok_or` had been calling on every hot path (21 of LVL's 650 samples).

Where the rest goes (`sample`, five runs of each row summed; `blaze3::compress` is the key):
- MAX, 928 samples: `compress` 339 and malloc/free about 250 under the global allocator. That run used `Alloc::global()`; in a frame the mallocs go, as the 395 to 314 ns change shows.
- DEFAULT, 962 samples: `compress` 273, `memset` 101, `Memory::region` 81, `Memory::write` 61, the step loop 77 plus `value_of` inlined 118.
- LVL, 1097 samples before enum facts: `compress` 251, `value_of` inlined 190, the step loop 171, `Memory::region` 43, `Memory::read` 38, layout lookups (`variant_of`, `variant_of_tag`, `Prim::bits`) about 45.
- LVL after, three runs, about 650 samples: `compress` 155, `value_of` inlined 136, the step loop 125, and no layout lookup. The per-evaluation setup is about 130: `Memory::region` 33 (growing fresh vectors), `memmove` 36, `insertion_sort` of the reads 22, `enter` 20, `Memory::allocate` 19.

**The per-evaluation setup was cut and was not the cause** (2026-11-16, three runs each):
- **What was cut.**
  - The machine's working vectors (`machine::Work`) and memory, with each allocation's pointer slots, now live in `Codes` and are emptied, not remade, per evaluation.
  - Reads are kept sorted as they are added.
  - Memory reads and writes of 1/2/4/8/16 bytes are fixed-size, and `zero_unwritten` goes a word at a time.
  - `run` is not inlined into `value_of`.
- **Result.** LEN went from 336 to 342 ns (2.1x), DEFAULT from 411 to 401 ns (2.5x), and LVL from 511 to 482 ns (3.0x). MAX is 304 ns (1.9x), POW 1.7x and CRC 1.3x.
- **Where MAX's 387 samples go:**
  - The key is 209: `RecordKey::of` 115, `instance_key` 94.
  - `run` is 39.
  - `value_of`'s own code is about 70. Its stack frame is over 4 KB, and it moves `Work` in and out.
  - The rest are allocation and record building.
- **The steps.** The dispatch costs about 12 ns per step against the 5 ns floor. It copies an 80-byte `Op` and returns `R<Flow>` through memory.
- **On the sum floor** (`examples/cost.rs` since 2026-11-16), one run at 03:42: MAX 1.7x, MASK 1.7x, LEN 1.8x, DEFAULT 2.1x, POW 1.4x, LVL 2.1x, CRC 1.5x. Under it LEN, DEFAULT and LVL had read 1.8x, 2.1x and 1.9x. The Mac was loaded: two more runs gave LEN 1.9x and 2.0x, DEFAULT 2.2x and 2.1x, LVL 1.9x and 2.2x.
- **Owed** (the coordinator stopped optimising here: at about 2x the floor the evaluator does not stand between the owner and tempo):
  1. Shrink `value_of`'s frame: move `evaluate` and `intern` out of line and keep `Work` borrowed, not moved. Profile: in MAX's 387 samples under `value_of`, its own code is about 70, mostly at entry, with a stack frame over 4 KB (0x11c0). The key is 209 (`RecordKey::of` 115, `instance_key` 94), more than half; `run` is 39.
  2. Make `Op` 16 to 32 bytes, with immediates in a side table. Disassembly: the 80-byte `Op` is copied per step; dispatch is about 12 ns per step in LVL against the 5 ns floor.
  3. Let `step` return a small `Flow`, with the error parked on the machine. Disassembly: `R<Flow>` is returned through memory and reloaded from the stack.
  4. Let a register hold a constant's scalar value, so a scalar `Const` needs no decode.

## Real code (examples/census.rs, 2026-11-16)

The census covers library/core, library/alloc and crates/blaze-std: 535 files, lowered by `lower_file` with macros expanded by name. One flat world per file. It took 0.94 s in all, with 1059 evaluations at 1.9 µs each.

The evaluator evaluated 1054 constants: 475 const items, 566 anonymous constants, 8 statics and 5 discriminants. It refused none of its own. The other 5 read a constant refused earlier.

The refusals upstream were sent to their owners:
- the expansion lane: 1118 doc comments emitted as anonymous constants (fixed in blaze-lower since), 74 `assert!` NonStringTemplate, and 6 others;
- names (blaze-scope): 556 UnresolvedPath, 541 associated consts (impl selection), 277 unknown type names or paths, and 73 Mismatch;
- the census's own flat world: 385 names declared twice through macro expansions.

On the op list (03:01, the same three trees, with blaze-lower's work since): 1227 evaluations, 1210 evaluated (475 const items, 566 anonymous constants, 161 discriminants, 8 statics). The 17 refused at evaluation all read a constant refused earlier; the evaluator refused none of its own. The const item, anonymous constant and static counts equal the run above; the discriminants went from 5 to 161 with the input, not the machine.

To rerun: `mixcheck.sh blaze-consteval run census <absolute dirs>`. `CENSUS_SHOW=<cause text>` prints every site of a cause.

## The hosted hello's constants (2026-11-16)

The ladder's hosted hello runs 70 functions on the program side (`port27/hosted-hello-functions.txt` in the notes; 67 of them Rust). Their bodies, read one by one, reach these constants:

- `bstd::guest::OFFERS`: a const block that writes fields of a copy of `Offers::NONE`.
- `<Io as Grant>::KIND` and `Kind::name`.
- The `static MANIFEST` that `#[bstd::main]` makes: `Manifest::new(&[Param::new("io", <Io as Grant>::KIND)])`, which runs `spec::is_name` and `grant::same` over the name's bytes (core's `str::as_bytes`, `<[u8]>::len`, `u8::is_ascii_digit` and `u8::is_ascii_alphanumeric`).
- `size_of::<HostTable>()` in `guest::checked`.
- `HELD` and `[0; HELD]`.
- `static CURRENT: Cell<*const Run> = Cell::new(ptr::null())`.
- `Status::FAILED` and `Status::REFUSED`, the `wire` codes, and `EXCHANGE_LEAST`.
- `MAIN_UNBOUNDED`, over `Duration::MAX`.
- `Limits::USUAL`, over `Duration::from_secs`.
- `NonZero::<usize>::MIN` and `u8::MAX`.

The per-file census cannot reach them: they are associated, cross-file or generic. So they are a test, `tests::hosted_hello_constants`, which states each body as bstd and core write it. Methods become free fns and generic calls become their instances, because the fixture has neither.

It found three things the evaluator refused, now closed:
- range patterns (`b'0'..=b'9'`), compiled to a compare and a branch per bound;
- the `ptr_metadata` intrinsic (a slice's or `str`'s `len`);
- the `aggregate_raw_ptr` intrinsic (`ptr::from_raw_parts`, so `ptr::null`).

`likely`, `unlikely` and `cold_path` were added too.

Two things are still open, and neither is the evaluator's:
- **Pattern types.** `Duration` holds `Nanoseconds`, and `NonZero<usize>` holds `NonZeroUsizeInner`. Both are `pattern_type!(u32 is 0..=999_999_999)` and its kin in this core, and `PNode` has no pattern type. So `MAIN_UNBOUNDED`, `Limits::USUAL` and `NonZero::MIN` cannot be checked. What the evaluator needs once `PNode` has one: layout as the base type plus its valid range, and `transmute` by bytes, which it already has.
- **A name-to-intrinsic map for the real worlds.** Done on 2026-11-16: `blaze_consteval::Intrinsic` is a re-export of `blaze_build::intrinsics::Which`, the one table (rule 30). It gained the rows `ptr_metadata`, `aggregate_raw_ptr`, `const_eval_select` and `ptr_guaranteed_cmp`. The code builder's lane (agent aa043a9dd60d38793) has since built them to code; `const_eval_select` is in the walk. Its run: "mixcheck test blaze-build: exit 0; 74 passed". The ladder's `ConstWorld` still has to answer `FnKind::Intrinsic(Which::named(name)?)`. It was told (agent af5a0de8abf69d263).

## Comptime on the item layer (design, 2026-11-16; rule 29)

Comptime and reflection are the one primitive that other features are libraries over (extensions.md section 4 "B"). `mark`, `bits`, `canonical_cbor`, `policy`, `memo`/`cache`, `holds` and `program_lifetime` are read or derived through them. comptime.md and reflect.md hold, except where a decision below says why another form is better.

1. **A body is compiled once into code, and that code is a record.** The code is a flat op list over numbered slots, made from a body at one ground instance. At compile time:
   - every type is grounded;
   - every adjustment is folded into the ops;
   - every literal is parsed;
   - every field offset, tag write and tag read is resolved through the unit's `Layouts`;
   - every callee is resolved to an instance.

   A local whose address is never taken is a register: a `u128` with an optional pointer. One whose address is taken is an allocation, as today. The record's key is the body's digest, the instance's arguments by digest and the target. Its read list holds the layout and type records it consulted. Every evaluation on every machine shares it (rule 52). An evaluation is that code plus its arguments, run by one loop over the ops. A call pushes a frame of slots, with no recursion in the host. The tree walk is deleted when the op list passes the suite. The target is all five cost rows under 2x the floor, quoted before and after.
2. **A type is a value by its digest.** A `TypeId` constant's bytes are the content digest of the ground type's structural key. A constant holding ids is then plain bytes, equal in every evaluation and on every machine. This replaces comptime.md's per-evaluation id list and its remapping (`own_type_id`), which a record shared across machines cannot carry. `name_of(id)` and `type_of!(id)` look the digest up in the unit's type records. A digest with no record is a `Need`, then a typed refusal.
3. **Reflection queries are records with read lists.** Each query is answered from the signature record of the item it names, keyed by that item's own signature digest, never the crate's:
   - `fields_of::<T>`, `variants_of::<T>`, a field's `INDEX`, `NAME` and `MARKS`;
   - a variant's `DISCRIMINANT`;
   - `fn_info::<F>` (its `code_id` is the body's digest);
   - `size_of`, `align_of` and offsets (layout records).

   The digest of every record a query read goes into the evaluation's read list. An edit to a struct's fields changes its signature digest, so exactly the evaluations that reflected on it fail their read-list check and run again. An edit to a body changes only `fn_info`'s `code_id` readers.
4. **`mark` is the one attribute primitive.** `#[blaze::mark(M)]` names a type `M`, resolved to its item, and is kept in the marked item's signature record. `Marked<M>` and `.marked::<M>()` are answered from it. `#[blaze::attr("..")]` folds into marks: a string is unchecked and a type is resolved (rules 30 and 76). `policy`, `bits`, `canonical_cbor` and `invariant` become marks read by libraries.
5. **Comptime is a task.** Its key is the code identity of what it runs, the arguments by digest and the target. Its result is a `ConstRecord`, and its read list is (3)'s. A record is valid by its key, or else by its read list checked against today's facts. Nothing invalidates by hand.
6. **`for const` and `x.[f]`.** The checker checks the body once, generically, against the field-binder bounds (the checker lane). Code for an instance unrolls the loop at compile, once per element of the domain query's answer, in declaration order. `x.[f]` becomes the field's resolved offset. The domain rule (which fields, the filters) is one function that the checker, the code compiler and the builder all call.
7. **`where const { COND }`.** The entry is `value_of` on the condition's anonymous constant at the instance's ground arguments, read as a `bool`. A panic is "does not hold", and its message is the reason shown. A `Need` is the goal's need. The answer is a record like any constant's: keyed by the condition's code identity and arguments, with a read list.
8. **`const static { .. }`** is an evaluation of its own with comptime rights. It may call non-const fns, `dyn` and fn pointers, and allocate on its const heap; heap the value reaches through shared, `Freeze` data becomes the record's own allocations. It refuses foreign calls (`Foreign`) and pointer addresses (`PointerAddress`) by name, as comptime.md says.
9. **`type_of!(desc)`.** A `TypeDesc` evaluated is a structural type, keyed by the digest of the evaluated description. Two equal descriptions are one type in every crate. A description that depends on itself is a typed `Cycle` naming the alias and the constant.
10. **Order:**
    1. the op list, with cost rows under 2x;
    2. type values and the reflection queries;
    3. `for const` and `x.[f]`;
    4. `where const`, for the sibling lane: until (2) it runs on what is already there, with `value_of`;
    5. `type_of!` and built structs.

    The probe is `crates/blaze-real/extensions` (`b_comptime_*`, `b_reflect`, `b_type_of`, `b_where_const*`). The real-code check is `library/core/src/reflect.rs`, evaluated.

## Not built

- inline `const` blocks: the body form has no node for them yet (range patterns are built, 2026-11-16);
- associated consts with generic parameters: `Def::Const` carries no arguments;
- drop glue;
- `const` trait impls;
- heap allocation in constants (`const_allocate`);
- `f16` and `f128`;
- comptime and reflection (extensions.md section 4 "B"): the next piece.

## For a successor (2026-11-16, third lane)

- State: the op list is the machine; the tree walk is deleted. `mixcheck.sh blaze-consteval test` passed 24 of 24 on it (2026-11-16 03:52; again with `Which` as `Intrinsic`, beside blaze-build's 73 and the ladder's tests, exit 0), and `lanecheck.sh blaze-consteval` said check ok. `value_of`, `eval_len` and `value_of_planted` take a `&mut Codes` that lives with the unit's ground table and `Layouts` (the fixture keeps all three in `unit`; blaze-ladder's `eval_consts` makes one).
- Where things are: `machine/code.rs` compiles (types `Op`, `Code`, `Codes`; `Machine::code`, `body`, the task enum `C`, patterns in `pattern`); `machine.rs` runs (`run`, `step`, `call`, `enter`) and keeps the value helpers, intrinsics and `intern`; `mem.rs` is the one-heap memory.
- Census: unchanged in kind. After enum facts: 1,206 evaluations, 1,201 evaluated, 5 refused only through a dependency, none refused by the evaluator itself. The count moved from 1,227 with the front end's tree.
- Floor: the sum of key and steps (Cost). On it, every row is 1.4x to 2.2x. The four owed cuts, with their profile numbers, are under Cost, "Owed"; the coordinator parked them.
- `Codes` now also holds the machine's working vectors (`machine::Work`) and its memory. One `Codes` serves one evaluation at a time.
- Done: discriminant facts checked once, as the proof type `code::EnumFacts`. `match`, `as` and `discriminant_value` all take it, and no `Layouts` lookup is left in a run. `eval_discriminant` (one variant, for the layout's `LayoutFacts`) still walks back to the nearest explicit discriminant: computing all of them would read later variants' records, which the layout of an earlier one must not need.
- Next (the coordinator, 2026-11-16):
  1. Answer the lanes that call the evaluator first: the checker's forms lane (agent a140bd4628d7499e9) and the ladder (agent ac55c4564581b206e). Both have the entry points.
  2. The hosted hello's constants: the state is in their section. The intrinsic map is done. Pattern types are the checker lane's next variant set (agent a998533e5efdf9d8e: a pattern type in `PNode`/`TyView`, with layout giving the base type and its valid range). When it asks, tell it this: a value of a pattern type is its base type's bytes; `transmute` to and from the base is by bytes and needs nothing new; and validity is not checked at evaluation, as for `rustc_layout_scalar_valid_range`.
  - Testing a change across crates: `mixcheck.sh` puts back only the named crate on another crate's green snapshot, so a set over two crates can fail there for no fault of its own. Check it in one locked step in /Volumes/Projects/blaze-green-1115: take the tree at `refs/green/blaze-ladder`, copy the changed files in, run `cargo test -p blaze-ladder -p blaze-build -p blaze-consteval --lib --tests`. Other lanes reset that worktree between commands.
  3. Comptime's type values and reflection records wait. `size_of`, `align_of` and `offset_of` on real generic types come first.
- Semantics the compile keeps on purpose: an assignment's right side is evaluated before its place; a match scrutinee is a place; a statement's temporaries end with it (`Mark`/`Kill`); a refusal names the operation's expression range, a panic the call's; `StepLimit` names no site. An or-pattern takes its first matching alternative, with no backtracking across alternatives; only a guard could tell the difference.
- The `where const` lane (agent a644b0eea2e247a89) was sent the entry point: `value_of` driven by `Settle`. A `Refusal::Panic` means "does not hold".
