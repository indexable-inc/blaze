---
status: record
summary: The differential fuzzer and the bugs it found.
date: 2026-10-20
---
# A differential fuzzer for blaze

Every harness before this one runs programs that somebody wrote. This one makes programs up.
`harness/fuzz/gen.py` writes valid, deterministic, terminating Rust programs from a seed, and
`harness/fuzz/fuzz.py` builds each one with cargo and with blaze under both backends, runs the
binaries and compares what they print. A case whose output differs, a function blaze leaves to
rustc, or a blaze crash is a finding; the driver reduces it and writes it to
`harness/fuzz/found/` with the seed that makes it again.

Branch `blaze-fuzz`, from `blaze-collate` at 6d12f21c86, with `blaze-collate` at ce52276cf5
merged in at the end of the first round (27920a4f69, see "The merge") and again at 3d47ed59bf
in the second (60bc3d6cfa). Everything below was measured on The four-backend runs of the second round are not the long run that was asked for. L11
(360 programs) ran before findings 17 and 19 were fixed and found them. L12 was stopped at 57
programs when it showed finding 21, and L13, on the binary with that fixed, was stopped at 60
programs when this work was wound down: 60 programs, 1,800 cases, 17,619 functions generated,
22,004 compiled under each of the four backends, no difference, no crash, nothing left to
rustc. So the head has 60 clean programs behind it under the four configurations, not 360,
and going by the earlier runs a problem shows about once in a few hundred programs. The
next thing to do is `fuzz.py run --seed L13 --first 60 --rounds 300 --cases 30 --backends
cranelift,llvm,llvm-O0,llvm-full`.

Linux x86-64 with Rust 1.99.0 and
LLVM 20.1.8, with a release blaze built with `--features llvm`, except the section "AArch64
under qemu", which is the same machine building for `aarch64-unknown-linux-gnu` and running
the binaries with `qemu-aarch64`. macOS, Windows and wasm were not run.

The second round added four things, each with its own section: two more LLVM configurations
in every run ("The long runs"), scenes in the generator ("Scenes"), generated edits
("Generated edits") and AArch64 ("AArch64 under qemu").

## Result

The fuzzer found twenty-one distinct problems in whole builds. Eighteen are fixed in blaze, two
are worked around (6 and 20, which I believe are bugs in LLVM, not in blaze), and one is open.
The generated edits found four more, in the daemon's incremental path, all fixed, and a fifth
in the last run (E5) that is open and not looked into; they are
in their own section. Each has a reduced case in `harness/breakers/`.

| # | What | Backends | Status | Breaker case |
|---|---|---|---|---|
| 1 | A panic's column was wrong for a parenthesised expression, for built-in indexing, and on a line with wide or zero-width characters | both | fixed, f1e4a5011f | `panic_columns`, `panic_column_width` |
| 2 | An index out of bounds inside a larger statement was reported at the statement, or at `<blaze>:0:0` | both | fixed, 80ca599faf | `panic_index_operand` |
| 3 | `assert!` of a condition longer than a line prints the condition on one line; rustc breaks it over several | both | open | `assert_message` |
| 4 | When an operand of an array, tuple, struct literal or call panicked, the operands already made were dropped in the wrong order relative to the statement's temporaries | both | fixed, dcb6b1fa7a | `unwind_operand_order` |
| 5 | A function with a C-convention aggregate argument passed on the stack whose size is not a multiple of eight was left to rustc (Cranelift panicked) | cranelift | fixed, ca26ccf5d1 | `stack_aggregate_odd_size` |
| 6 | An array of repeated `[i128; 5]` rows came out with part of it unwritten | llvm | gone, SLP back on 2026-10-23 | `repeat_rows` |
| 7 | Before the 2024 edition, the temporaries of the tail of an `if` branch were dropped at the end of the enclosing statement instead of the end of the branch | both | fixed, fa04742c8d | `if_branch_temporaries` |
| 8 | A closure that matches on a field of a variable without binding anything, and uses another field, was left to rustc (`UnresolvedUpvar`) | both | fixed, 94141b5783 | `uncaptured_scrutinee` |
| 9 | `let _ = items[at]` and `match items[at] { _ => .. }` did not check the index, so an index out of bounds did not panic | both | fixed, a5360940ee | `unread_index` |
| 10 | When the pattern of a `let ... else` did not match, the value and the temporaries of the initializer were dropped after the `else` ran instead of before | both | fixed, e04c495c9c | `let_else_temporaries` |
| 11 | An array whose pattern would move an element out, where the pattern did not match, was dropped from the back instead of front to back | both | fixed, e04c495c9c and a1f0466b5d | `let_else_temporaries`, `unmatched_array_drop` |
| 12 | Before the 2024 edition, when a `for` loop was the tail of a block, the temporaries of what it iterates over were dropped after the block's locals instead of where the loop ends | both | fixed, 876f964f06 | `for_tail_temporaries` |
| 13 | Before the 2024 edition, the temporaries of what an `if let` matches on were dropped at the end of the branch instead of with those of the expression the `if` is in | both | fixed, 302a786299 | `if_let_temporaries` |
| 14 | When an arm bound part of an array by value (`[_, middle @ .., _] if guard`) and its guard was false, the elements put back into the array were never dropped | both | fixed, 49c4fddd1f | `guard_subslice_restored` |
| 15 | An array matched by an arm that would move elements out, where rustc can tell from the arms before it that no value reaches that arm, is dropped from the back instead of front to back | both | fixed, 20d97d4d73 | `unreachable_arm_move` |
| 16 | When an operand of an aggregate or call panicked and the expression was the tail of a block, the operands already made were dropped in the wrong order relative to the temporaries (finding 4 again, outside a statement) | both | fixed, 27a94f2770 | `unwind_operand_tail`, `unwind_operand_order` |
| 17 | When an operand of an aggregate that is itself an operand of another aggregate panicked, the outer aggregate's finished operands were dropped before the inner one's | both | fixed, 5fba09c841 | `unwind_nested_operands` |
| 18 | Before the 2024 edition, when an operand of the tail of a block panicked, the block's locals were dropped before the tail's finished operands | both | fixed, 2368d6be98 | `unwind_tail_after_local` |
| 19 | When the body of a loop inside another loop panicked, what the inner loop iterates over was dropped before what its body held | all four | fixed, cb41f78a52 | `unwind_nested_loops` |
| 20 | On AArch64, a closure with many conditionally dropped values died in `free` while unwinding after a caught panic | llvm `-O2` fast emit, AArch64 only | worked around, 4602a6909f | `unwind_many_flags` |
| 21 | Finding 15 again: an arm that is only an or-pattern, behind an arm whose test also decides a later arm, hid that the later arm is never reached, and the array was dropped from the back | all four | fixed, e45ffdff69 | `unreachable_arm_move` |

Numbers 4, 6, 7 and 9 to 21 are miscompiles: the program ran and did the wrong thing. Numbers 1 to 3
are wrong panic text. Numbers 5 and 8 are functions blaze refused, with the reason stated, so the
program was still right.

## The long runs

All with `python3 fuzz/fuzz.py run --seed S --rounds N --cases 30 --jobs 8 --reduce 600 --out DIR`,
counts from `fuzz.py report DIR`. L1 to L10 ran two backends, Cranelift and LLVM at `-O2` with
fast emit. From L11 on a run has four (`--backends cranelift,llvm,llvm-O0,llvm-full`): those
two, `llvm-O0` (`BLAZE_OPT=0`, fast emit) and `llvm-full` (`BLAZE_OPT=2 BLAZE_EMIT=optimized`,
LLVM's own instruction selection at `-O2`). Every backend builds the same programs, so the
number compiled is the same for each; the two "Compiled" columns are then the first two. "Generated" counts the functions the generator
wrote; "compiled" is the number of functions blaze's own report says it compiled, which also
has the prelude's functions and leaves out generic functions nobody calls. Each run used the
blaze of the time, so the earlier ones show the bugs that were then fixed.

| Run | Seed, rounds | blaze | Programs run | Functions generated | Compiled, Cranelift | Compiled, LLVM | Differences | Crashes | Left to rustc |
|---|---|---|---:|---:|---:|---:|---|---:|---|
| L1 | `L1`, 136 | with fixes 1, 2, 4 | 136 | 39,720 | 41,089 | 38,984 | Cranelift 4, LLVM 7 (all checksum; findings 6, 7) | 0 | Cranelift 12, `backend panic: StructArgument size is not properly aligned` (finding 5); LLVM 0 |
| L2 | `L2`, 119 (stopped) | with fixes 1 to 7 | 119 | 35,230 | 34,195 | 34,195 | 0 | 0 | 1 per backend, `mir: UnresolvedUpvar` (finding 8) |
| L3 | `L2`, 360 | with fixes 1 to 8 | 360 | 105,869 | 102,426 | 102,426 | 5 per backend: 4 checksum (findings 10, 11), 1 panic location (finding 9) | 0 | 0 |
| L4 | `L4`, 120 | e04c495c9c | 119 | 35,679 | 33,854 | 33,854 | 0 | 0 | 0 |
| L5 | `L5`, 150 | a1f0466b5d | 150 | 44,031 | 43,069 | 43,069 | 0 | 0 | 0 |
| L6 | `L6`, 210 | a1f0466b5d | 209 | 62,034 | 59,182 | 59,182 | 0 | 0 | 0 |
| L7 | `L7`, 360 | 876f964f06 | 360 | 106,202 | 102,898 | 102,898 | 8 per backend, all checksum (findings 13, 14, 15) | 0 | 0 |
| L8 | `L8`, 360 | 49c4fddd1f | 360 | 105,649 | 102,081 | 102,081 | 1 per backend, checksum (finding 16) | 0 | 0 |
| L9 | `L9`, 360 | 27a94f2770 | 360 | 106,116 | 101,848 | 101,848 | 0 | 0 | 0 |
| L10 | `L10`, 60 | 27920a4f69 (merged) | 60 | 17,297 | 16,734 | 16,734 | 0 | 0 | 0 |
| L11 | `L11`, 360, four backends | 8f21942505 (merged) | 360 | 106,339 | 102,475 | 102,475 | 2 under each of the four, checksum (findings 17, 19) | 0 | 0 |
| L12 | `L12`, 57 (stopped), four backends | 4602a6909f | 57 | 16,727 | 21,388 | 21,388 | 1 under each of the four, checksum (finding 21) | 0 | 3 under each of the four: 2 `intrinsic powif32`, 1 `shares an async block with a function left to rustc` (the same function) |
| L13 | `L13`, 60 (stopped), four backends | e45ffdff69 | 60 | 17,619 | 22,004 | 22,004 | 0 | 0 | 0 |

L9 is the run on the last binary before the merge described below: 360 programs, none rejected,
10,800 cases, 106,116 functions generated and 101,848 compiled by blaze under each backend, no
difference, no crash, nothing left to rustc. L5 and L6 were meant to be that run (106,065
functions together, clean), and so were L7 and L8; each time a run that was otherwise clean
turned up one to three more drop-order bugs, which were fixed before the next. So the
honest reading is that about one program in a few hundred still found something up to L8,
and L9 is the first full-size run that found nothing.

L3 ran the seed of L2 again from its first round, so the 119 programs of L2 are the
first 119 of L3 and should not be counted twice; without L2 the runs are 2,116 programs and
622,597 generated functions. In L1 the two backends compiled different numbers of functions
because of the functions left to rustc and because the count is per build of the daemon's
report; I did not look further into that difference. Smaller runs before L1 (seeds `s1` and `s2`,
while the generator was being written) found the first findings, the `s1-` and `s2-` files
under `found/`; they are not counted here.

Two of the 2,116 programs were not compared, both through faults of the generator and not of
blaze, and both fixed:

- `L4-51` was rejected by rustc (E0502): a closure borrowed a trait object and the argument
  next to it called a `&mut self` method of the same object. The argument of a call no longer
  changes what the closure before it borrows (a1f0466b5d); `fuzz.py repro L4-51` then builds,
  301 functions generated, 299 compiled by blaze under each backend, no finding.
- `L6-176` built, but rustc's binary aborted: the program called `offset_from` on pointers to
  `()`, which panics for a zero-sized element, in a function that cannot unwind. The generator
  now leaves the distance out for such an element. With that, `fuzz.py repro L6-176` ran and
  showed finding 12 in case `c3` under both backends.

## Prior art

I read how the existing generators work before writing one. None of them was run here.

- rustlantis generates MIR directly, as `#![feature(custom_mir)]` source. That needs a nightly
  compiler, and it enters the compiler below the level blaze starts from: blaze lowers source
  through its own HIR and MIR, so a program written as MIR does not test that lowering, and
  1.99 stable rejects it.
- rustsmith generates source programs, but from a small language. Going by its own description
  its programs have no trait objects, closures by capture kind, slice patterns, raw pointers, `repr`
  attributes, `catch_unwind` or observable drop order, which is most of the list this task
  asks for. It is also a Kotlin program, which would add a JVM to the harness.
- From csmith I took swarm testing. Every case turns a random fifth of the constructs off and
  weights the rest differently (`weight` in `gen.py`), so that cases differ in kind and not only
  in detail.

So the generator is new: `harness/fuzz/gen.py`, 2,565 lines of Python, no dependencies.

## The generator

A program is `prelude.rs` followed by one module per case. A case is a set of types, traits and
helper functions and a `run` function. Everything a case computes is fed to one 64-bit checksum
(the `Mix` trait in `prelude.rs`, implemented for every type the generator uses), and `main` runs
each case inside `catch_unwind` and prints one line per case: the name, the checksum, and whether
it ended or panicked. Panic messages go to stdout too through a panic hook, so the place a
panic is reported at is compared as well. The source of case N of seed S depends only on (S, N),
so one case can be made again alone.

What keeps a program free of undefined behaviour and of anything two correct compilers may
disagree on:

- Arithmetic is wrapping, checked, saturating or overflowing. Plain `+`, `/`, `<<`, indexing and
  `unwrap` appear only where the panic is caught, and the values are chosen so that it is known
  whether they panic.
- No address, NaN bit pattern, sign of a zero from `min`/`max`, or hash order is observed. Maps
  are `BTreeMap`.
- Raw pointers stay inside the object they came from and read only what was written. Unions and
  `MaybeUninit` are read only through the field that was written.
- Every loop has a counter the generated code cannot assign. Calls go only to functions defined
  earlier, recursion counts down, and a case's work is bounded by an estimate.
- Half the programs are built as edition 2021 and half as 2024, because drop order differs.

### Coverage

Each construct the generator emits is counted by name; there are 400 counters. The
table groups them by the list in the task. The counts are from run L3
(360 programs; `fuzz.py report DIR --coverage` prints every counter).

| Construct | Times generated | Counters |
|---|---:|---|
| Type choices: an integer, any of the twelve widths | 16812988 | 1 |
| Type choices: `i128`/`u128` only (a separate choice, in addition to the row above) | 17346223 | 1 |
| Wrapping, checked, saturating, overflowing arithmetic | 209515 | 11 |
| Shifts | 63324 | 4 |
| Other integer operations (bits, bytes, ilog, isqrt, clamp) | 60992 | 7 |
| Casts (`as`, `From`, `TryFrom`) | 135476 | 10 |
| Floats | 54410 | 7 |
| `bool` and `char` | 72453 | 9 |
| Tuples | 148119 | 3 |
| Structs (plain, generic, update syntax) | 125808 | 4 |
| Enums with data, fieldless enums, niches (`NonZero`, `Option<Box>`) | 107278 | 4 |
| `repr(C)`, `repr(u8)` and other integer reprs, `repr(align)`, packed | 33138 | 9 |
| Arrays | 119355 | 8 |
| Slices | 42487 | 12 |
| References | 98679 | 5 |
| Raw pointers, unions, `MaybeUninit`, transmute | 167476 | 18 |
| Patterns: literals, ranges, bindings, `@`, or-patterns | 562826 | 10 |
| Patterns: slices | 181844 | 8 |
| Patterns: guards | 88300 | 1 |
| `match`, `if let`, `let else` | 346107 | 8 |
| Loops (`loop`, `while`, `while let`, `for`) | 78612 | 5 |
| Labelled `break`/`continue`, `break` with a value, labelled blocks | 81987 | 3 |
| Closures: capture by reference | 48316 | 4 |
| Closures: capture by mutable reference | 29516 | 3 |
| Closures: capture by move, `FnOnce`, returned and boxed closures | 82087 | 8 |
| Generics (functions, structs, enums, const generics, bounds) | 246508 | 10 |
| Traits (impls, default methods, derives, qualified calls) | 56231 | 8 |
| Trait objects | 46602 | 6 |
| `Option` | 160135 | 15 |
| `Result` and `?` | 47797 | 8 |
| `Box` | 21238 | 2 |
| `Vec` | 138732 | 23 |
| `String` and `str` | 95946 | 31 |
| Iterators | 185441 | 27 |
| Drops with observable order | 28518 | 8 |
| Panics caught with `catch_unwind` | 73220 | 16 |
| Function pointers | 33247 | 4 |
| Calls by argument count (0 to 8 and more) | 84088 | 8 |
| ABI: scalar integer argument or return | 72693 | 2 |
| ABI: scalar float argument or return | 11957 | 2 |
| ABI: integer pair | 8665 | 2 |
| ABI: float pair | 357 | 2 |
| ABI: mixed float/integer pair | 844 | 2 |
| ABI: in memory (large aggregate) | 40817 | 2 |
| ABI: zero-sized | 6527 | 2 |
| `extern "C"` functions | 7802 | 1 |
| Statics, consts, `size_of`/`align_of` | 41433 | 8 |
| Formatting (`Debug`, `Display`), `BTreeMap` | 28517 | 3 |

"Times generated" is the sum of the counters in the row, and "Counters" how many there are.
The first two rows count each time the generator picked a type, including the picks it made
while looking for a type that fits a place, which is why they are so large; every other row
counts constructs written into a program. Four counters are in two rows (`closure.by_reference` under references and closures,
`cast.enum_to_int` under casts and enums, `raw.packed_field` under raw pointers and reprs,
`generic.const_array` under arrays and generics), so the column does not add up to a total. 49 of the 400 counters are in no row because they are not on
the task's list or are too general to mean anything (`stmt.let`, `expr.literal`, `type.bool`
and the like).

The integer types by how often their name appears in the source of the first twenty of those
programs (5,827 functions): `i8` 8,321, `i16` 5,661, `i32` 5,669, `i64` 6,059, `i128` 14,784,
`isize` 5,388, `u8` 9,049, `u16` 5,682, `u32` 6,463, `u64` 5,774, `u128` 14,686, `usize` 6,566,
`f32` 17,524, `f64` 17,115.

What the generator does not make: threads, macros of its own, SIMD intrinsics, trait objects
with generic methods, specialization-like overlap, and any I/O other than the checksum lines.
`async`, `dyn Any` and inline assembly come only from the scenes below. Float pairs returned in two SSE registers are generated
least of the ABI classes (see the table).

### Scenes

The second round added `harness/fuzz/scenes.py`: 25 statements that are each a small program
of their own. A scene is one block that declares what it needs inside itself, takes its
numbers, shapes and order from the seed, and feeds what it computes and what it drops to the
checksum. They are for what the free-form statements reach rarely or not at all, in the order
the task gave:

| Scenes | What they do |
|---|---|
| `unwind.drop_panics`, `unwind.partial_move`, `unwind.operands`, `unwind.loop`, `unwind.closure`, `unwind.replace` | A `Drop` that panics while its neighbours are dropped; a struct, tuple or array with some fields moved out when the panic comes; operands of nested aggregates and calls; a panic in the body of a loop that holds values; in a closure that owns its captures; between `mem::replace`/`take` and the write back |
| `async.across_await`, `async.dropped_pending`, `async.nested`, `async.panic_in_poll`, `async.boxed_futures` | `async fn`s with locals of several sizes held across awaits, polled by hand with a no-op waker; a future dropped after zero, one or more polls, with what it holds dropped in order; futures awaiting futures; a panic inside `poll`, caught, and the future dropped after; `Pin<Box<dyn Future>>` in a vector |
| `dyn.upcast`, `dyn.any`, `dyn.unsized_generic` | `&dyn Sub` to `&dyn Super`, in `Box` and `Rc` too, through three traits; `dyn Any` downcasts that succeed and fail; generic code over `T: ?Sized` given slices, `str` and trait objects |
| `closure.made_in_loop`, `closure.fields` | Closures made on each turn of a loop, by reference, by mutable reference and by `move`, kept and called afterwards; closures that capture single fields of a struct |
| `float.casts`, `float.operations` | Every float to every integer and back at the edges (NaN, infinities, the two zeros, the bounds of each width, subnormals, 2^24 and 2^53 plus one); arithmetic, rounding, `mul_add`, `powi`, comparisons, classification and formatting on those |
| `wide.operations`, `wide.arguments` | 128-bit arithmetic of every kind with operands around 2^64 and 2^127, shifts by 0 to 127, division and remainder; 128-bit values as the second to ninth argument and in structs |
| `niche.enums` | `Option` of references, boxes, `NonZero`, `char`, `bool`, fieldless and nested enums; their sizes, and reading each back after writing each variant |
| `match.arms_in_the_way` | The matches of finding 15: arms that move out behind arms no value passes |
| `static.pointers` | Statics that hold references to other statics, to their fields and to slices of them, and function pointers |
| `repr.variety` | `repr(C)`, `repr(u8)` to `repr(i64)`, `repr(C, u8)`, `repr(align(N))`, `repr(packed(N))`, `repr(transparent)`: sizes, alignments, field offsets, and values through them |
| `asm` | A few instructions of register arithmetic through `asm!`, one form for x86-64 and one for AArch64 |

A function has a scene with a probability the program draws once, so some programs are full
of them and some have none.

The first sixteen programs with scenes (`--seed S1 --rounds 16`, blaze at 20d97d4d73) had 14
differences under each backend, no crash and nothing left to rustc. All were findings 17 and
18, both in unwinding with operands in flight; after those two fixes the same sixteen
programs are clean. Coroutines other than `async` are not in stable Rust and are not
generated.

## The driver

    cd harness
    export BLAZE_BIN=/path/to/release/blaze-engine
    python3 fuzz/fuzz.py smoke                              # 2 programs of 25 cases, fixed seed, for CI
    python3 fuzz/fuzz.py run --seed L2 --rounds 360 --jobs 8 --out DIR
    python3 fuzz/fuzz.py repro L2-1 c27                     # one program again, or some of its cases
    python3 fuzz/fuzz.py check fuzz/found/*.rs              # every reduction against this blaze
    python3 fuzz/fuzz.py report DIR --coverage              # the counts of a run

A round is one program: 30 cases by default, about 290 functions. It is one cargo project built
through `Fixture` from `difftest.py`, once by cargo and once by blaze per backend
(`--backends cranelift,llvm`; llvm runs with `BLAZE_OPT=2 BLAZE_EMIT=fast`), so a round costs
one cargo build and two blaze builds. Rounds are split over `--jobs` worker processes; round N
of a seed is the same program whatever the number of jobs.

A finding is one of: a case whose lines differ (told apart as checksum, panic location, panic
message, whether it panics, or the program dying), a function that blaze's report lists as skipped, or a blaze build that did not finish.
A cause seen before in the same run is counted and not reduced again. Reduction is delta
debugging in two steps: first over the cases of the program, then over the lines of the one
case left, by brace-balanced units, keeping a candidate only when rustc still accepts it and the
same kind of finding with the same key still shows. `--reduce` bounds the seconds spent per
finding. The result goes to `fuzz/found/<kind>-<key>-<seed>-<case>.rs` with a header that names
the seed, the edition, the backend and both outputs.

The smoke run is in CI (`.github/workflows/blaze.yaml`, in the Linux harness job after `keys.py`,
per backend, on x86-64 only since nothing else was fuzzed). On this
machine, with nothing else running, it took 57 s for both backends with the binary of 27a94f2770
and 89 s with the merged one
(`time python3 fuzz/fuzz.py smoke`): 475 functions generated, 505 compiled by blaze per
backend, no finding.

## The findings

**1. Panic columns.** blaze took the column of a panic from the start of the expression
without its parentheses, reported built-in indexing at the bracket where rustc reports it at
the start of the base, and counted columns in characters where rustc counts display width (a
tab is four, a wide character two, a combining mark zero). The fix distinguishes built-in
indexing from an overloaded `Index` and counts width with `unicode-width`, the crate rustc
uses. Reductions: `differs-panic-location-s1-2-c13.rs`.

**2. Index out of bounds inside a statement.** When the index expression was an operand of a
larger statement (an argument of `assert!`, the value of a `let` with a conversion around it),
the panic was reported at the whole statement or at `<blaze>:0:0`. The bounds check now carries
the span of the index expression. Reductions: `differs-panic-location-s2-2-c27.rs`,
`s2-3-c10`, `s2-28-c1`, `s2-36-c24`, `s2-43-c12`.

**3. `assert!` message (open).** The message of a failed `assert!` is the condition as
rustc's pretty printer writes it back, and that printer breaks a condition longer than a line
over several lines. blaze writes it on one line. Matching it needs `rustc_ast_pretty`, which
is not a crate blaze can depend on, or a port of its line-breaking algorithm. The generator
keeps `assert!` conditions short so that this does not drown other findings. The case is in
`OPEN` in `breakers.py`.

**4. Drop order when an operand panics.** In
`let x = [Loud(1), sum(&Loud(20), &Loud(30)), Loud(3), none().unwrap()];` rustc drops, on the
unwind, 3, 50, 1 and then 30, 20: the operands belong to the array expression, and the
`&Loud(..)` temporaries live to the end of the statement. blaze dropped 1 last. The same
happened for tuples, struct literals and call arguments. In a block's tail expression both
kinds of temporary are in one scope and the order is plain reverse creation, which blaze
already did, so the fix applies to statements only. Reduction: `differs-checksum-s2-23-c24.rs`.

**5. Odd-sized stack aggregate.** An `extern "C"` function whose aggregate argument no longer
fits in registers gets it on the stack. blaze gave Cranelift the aggregate's exact size, and
Cranelift asserts a multiple of eight, so the function was left to rustc with `backend panic:
StructArgument size is not properly aligned`. The slot is now rounded up, as the System V ABI
says. Reductions: the three `left-backend-panic-structargument-*.rs`. This one was hit 12
times in 136 programs.

**6. LLVM's SLP vectorizer (worked around).**
`[[-18i128; 5], [524287; 5], ...]` built under LLVM at `-O2` had rows whose later elements were
never written. The IR blaze gives LLVM is right: `opt-20 -O2 -opt-bisect-limit` on the dumped
module puts the change at the SLP vectorizer pass, which turns the stores into vector stores
with poison lanes. This is LLVM 20.1.8 miscompiling valid IR. blaze now turns the SLP
vectorizer off (`-vectorize-slp=false`); the loop vectorizer stays on. I did not measure what
that costs in generated code speed, and I did not check whether a later LLVM has the bug.
rustc 1.99 uses its own LLVM and did not show it on the same source. Reductions:
`differs-checksum-L1-45-c17.rs`, `L1-77-c19`, `L1-142-c12`, `s2-49-c12`.

2026-10-23: with the SLP vectorizer on again, `repeat_rows` and the four reductions match
rustc on x86-64 and AArch64 at `-O2` and `-O3`, and so do 300 fresh programs on x86-64 at
`-O2` (`fuzz.py run --seed slp1 --rounds 300 --backends llvm`). The IR blaze gives LLVM has changed since (copies
are `memcpy`s, the pipeline is ThinLTO's); LLVM 20.1.8 is the same. The pass is on again, as
rustc runs it: nbody went from 1.21 to 0.98 times release on x86-64 and from 1.28 to 1.04 on
AArch64.

**7. Temporaries of an `if` branch's tail, before 2024.** Before the 2024 edition the
temporaries of a block's tail expression live as long as those of the expression the block is
in. blaze applied that to every block. A branch of an `if` is where rustc ends them anyway, so
in `seen(&if c { a } else { pick(&Loud(4), &Loud(6)).clone() })` rustc drops 6 and 4 before
`seen` runs and blaze dropped them after. Each branch now has its own scope. Match arms, loop
bodies and the right side of `&&`/`||` were checked by hand and were already right. The
breakers crate is edition 2024, so the case lives in a new 2021 crate,
`harness/breakers/edition2021`. Reductions: `differs-checksum-L1-12-c3.rs`, `L1-98-c28`.

**8. Uncaptured match scrutinee.** In `|| match x.1 { (_, _) if g => a, _ => x.0 }` nothing
reads `x.1`, so the closure captures `x.0` only. blaze's MIR still had a fake read of `x.1`,
found no capture to map it to and gave up on the closure and, through it, on its callers.
Fake reads of places that are not captured are now removed; rustc does not emit them either.
Reduction: `left-mir-unresolvedupvar-L2-1-c27.rs`.

**9. Index of an element that is not read.** blaze checks an index where it computes the
address of the element, and it made no code at all for a place that is matched on by a
pattern that binds and tests nothing. So `let _ = items[at];`, `let (_, _) = items[at];` and
`match items[at] { _ => .. }` went on with `at` out of bounds where rustc panics. Such a place
is now still computed when it has a run-time index. The breaker has it for arrays, slices, a
field, a deref, a nested index and inside a closure. Reduction:
`differs-panic-location-L2-154-c15.rs` (the driver called it a panic location because the next
panic in the case was a different one).

**10. `let ... else` temporaries.** When the pattern of `let PAT = EXPR else { .. };` does not
match, rustc drops the value of `EXPR` and its temporaries, also the ones that would have
lived to the end of the block (`let (Loud(1), b) = &(Loud(n), Loud(5)) else ..`), and then runs
the `else`. blaze ran the `else` first and dropped them on the way out of it. They are now
dropped where the `else` starts. Reductions: `differs-checksum-L2-354-c5.rs`, `L2-372-c9`,
`L2-431-c24`.

**11. Order in which an unmatched array is dropped.** rustc drops an array that has lost
elements to a pattern from the back, and an array that is whole front to back, and it decides
which at compile time: where a drop is, can an element have been moved out on some way there.
blaze's MIR has no such analysis; it keeps flags at run time and used the from-the-back order
for every drop of a local that some pattern in the function moves out of. So
`let [a, Loud(7)] = [Loud(1), Loud(n)] else { .. }` with `n` not 7 dropped `n` and then 1. There
is now a small forward analysis over the MIR (`MirBody::parted_at_block_ends`): a drop that no
move out of a part of the local can reach drops the local as one value. That fixed `let else`
and `match`. `if let` needed one more change: blaze lowered `if let` to a `bool` and a switch
on it, which put the matched and the unmatched way through one block and so lost what the
analysis needs. An `if` whose whole condition is a `let` now goes straight from the pattern to
its branches. A let chain (`if let A = x && c`) still goes through the `bool`. I tried one by hand (a
four-element array pattern followed by `&& on`, with the pattern matching, not matching, and
matching with `on` false) and it printed what rustc's build prints under Cranelift; the
fuzzer does not generate let chains, so that is the only evidence for them.
Reduction: `differs-checksum-L2-463-c8.rs`, and the three of finding 10.

**12. Temporaries of a `for` in tail position, before 2024.** rustc writes `for` as a `match`
on the iterator around a `loop` and ends the temporaries of the whole thing where the loop
ends. That only shows before the 2024 edition and when the loop is the tail of a block (a
function body that ends in a `for` is one): the temporaries of any other tail are dropped
after the block's locals, those of a `for` before. blaze treated the loop as any other tail.
The `match` a `for` is written as now has a scope of its own for its temporaries. It was found
late because it needs a `for` in tail position whose iterable makes a temporary with a `Drop`,
in a 2021 program, and it came out of `L6-176` only after the generator fault in that
program was fixed (see the long runs). Reduction: `differs-checksum-L6-176-c3.rs`.

**13. Temporaries of an `if let`, before 2024.** Before the 2024 edition the temporaries of
the expression an `if let` matches on are not the `if`'s: they live as long as the temporaries
of the expression the `if` is in, so to the end of the statement, or past the block's locals
when the `if let` is the tail of a block. Only what the pattern binds ends with the branch.
blaze dropped all of it at the end of the branch, the 2024 rule. Before 2024 the scrutinee's
temporaries are now handed to the scope around the `if`. `L7` hit this in six programs (rounds
50, 81, 87, 171, 274 and 290). Reductions: `differs-checksum-L7-50-c6.rs`, `L7-87-c6`,
`L7-81-c9`, `L7-171-c9`.

**14. A subslice put back after a failed guard.** In
`match array { [_, middle @ .., Loud(3..=40)] if on => .., _ => .. }` rustc moves the middle
elements into `middle` before it runs the guard, and when the guard is false moves them back
so the next arms see the array whole. blaze did both moves, but its note of which elements of
the array are gone was only cleared for an assignment to a field or one element, not to a
subslice. The elements were back and still marked as moved, so the array was dropped without
them and their destructors never ran. An assignment to a subslice now clears the note for
each of its elements, the same set a move out of the subslice sets. Both editions, both
backends. `L7` hit it in one program (round 174). Reduction:
`differs-checksum-L7-174-c9.rs`.

**15. An arm that moves and that rustc's tree does not lead to.** In

    match items { [.., Loud::Char(_)] => 1, [Loud::Char(..), middle @ .., Loud::Char('Z')] => 2, _ => 3 }

the second arm can never be taken: the first takes every array that ends in a `Char`. rustc
builds one tree of tests for all the arms, the second arm's bindings end up in a block no
edge leads to, and its drop analysis sees an array nothing is moved out of and drops it front
to back. blaze tries the arms one after the other, so its MIR has a path through the second
arm's move, and the array was dropped as one that may have lost elements: from the back.

What decides is not whether a value can reach the arm but whether rustc's tree has an edge
to it, and those differ. I measured with rustc alone (`RUSTC_BOOTSTRAP=1 rustc
-Zdump-mir=FN -Zdump-mir-dir=DIR` writes the MIR as built, which is the ground truth here):
an arm behind an overlapping range, an arm behind a wildcard arm with a guard, and an arm
after arms for every variant are all unreachable for any value and all still have an edge,
so the array is dropped from the back; an arm covered through an or-pattern, through a
nested pattern or through an integer literal has none.

So the fix follows rustc's construction. `mir/lower/match_tree.rs` (about 490 lines) turns
each arm into what rustc calls a candidate, the list of places its pattern tests, and sorts
the candidates the way `rustc_mir_build`'s match builder does: take the first test of the
first candidate, sort the candidates the outcome says something about into its branches,
leave the rest for after, expand or-patterns where rustc does, and stop at a candidate that
matches without a guard. It builds nothing; it only says which arms are reached. That is
the part of rustc's roughly 4,000 lines of match lowering that decides reachability, and
it was contained. blaze still lowers the arms one after the other; the bindings of an arm
the tree does not reach go into a block listed in `MirBody::unreached`, and the analysis of
what may be moved out where a drop is does not enter those blocks. If the simulation is
wrong about an arm, the cost is the order of a drop: every part is still dropped once, by
its flag. A pattern with something the simulation does not follow (a constant named by a
path, a deref pattern) makes every arm of that `match` count as reached, as before.

The breaker case has nine functions, the six that rustc drops front to back and three it
does not, and matches rustc under both backends. `L7` hit this in one program (round 230).
Reduction: `differs-checksum-L7-230-c24.rs`.

**16. Operands of a tail when a later one panics.** The fix of finding 4 moved the operands
of an aggregate or call in front of the statement's temporaries on the way out of a panic,
and only did so inside a statement. `L8-231` had the same thing in the tail of a closure's
block, in a 2021 program. I measured what rustc does for each position (a closure with
`(Loud(1), sum(&Loud(20), &Loud(30)), Loud(3), none().unwrap())` as a statement, as the tail
of a block, as the body without braces, and nested in another call, in both editions) and
the rule is: operands belong to the expression they are operands of, temporaries to the
nearest expression that ends temporaries, and each scope is dropped in the reverse of the
order its values were made. So the operands go first (`3, 50, 1, 30, 20`) everywhere except
where the expression itself ends its temporaries, which is the body of a closure without
braces and, from the 2024 edition, the tail of a block; there everything is in one scope
(`3, 50, 30, 20, 1`). blaze now keeps the set of those expressions and applies the fix of
finding 4 to every other aggregate or call. Reduction: `differs-checksum-L8-231-c11.rs`.

**17. Operands of an aggregate inside an aggregate.** Found by the first run of the widened
generator (`S1-1`, and `L11-133` under all four backends). In

    (Noisy(27), Duo { a: Noisy(39), b: trip(armed, Noisy(15)) })

when `trip` panics rustc drops `39` and then `27`: the inner literal's operands are newer.
blaze dropped `27` first. The lowering that puts operands in front of temporaries (findings 4
and 16) ran once for the inner literal and again for the outer one, and the second pass
counted the inner operands as temporaries of the outer and moved them. It now remembers
which locals are operands and keeps them in the order they were made. Reductions:
`differs-checksum-S1-1-c22.rs`, `differs-checksum-L11-133-c4.rs`.

**18. The tail of a block and the block's locals.** Also from the widened generator
(`S1-2`, `S1-4`, `S1-6`, `S1-11`, `S1-13` were all this). In a 2021 program

    let _v = { let _local = Noisy(57); (Noisy(58), trip(armed, Noisy(47))) };

when `trip` panics rustc drops `58`, then `57`. Before 2024 the temporaries of a block's tail
outlive the block's locals, and blaze handed everything the tail made to the enclosing scope,
its operands too, so `57` went first. Only the temporaries are handed over now; the operands
stay in the block. I checked rustc's order with this program under both editions: it is
`58, 57` in both, and blaze was already right in 2024. Reduction:
`differs-checksum-S1-2-c7.rs`.

**19. Nested loops.** `L11-165`, under all four backends, with the generator as it was before
the scenes. In

    for round in 0..2 {
        for item in vec![Loud(1), Loud(2), Loud(3)] {
            let held = Loud(40);
            trip(&item);
        }
    }

when `trip` panics rustc drops `held`, then `item`, then the rest of the vector: the scopes
end from the inside out. To order the drops of a landing pad blaze ranked the locals by which
blocks could reach which, and inside a loop that says nothing, because the body of the inner
loop and its exit each lead to the other through the outer loop; the iterator came out first.
The lowering now writes down the order in which scopes end as it closes them
(`MirBody::scope_ends`) and the drops are sorted by that; the ranking by reachability is
deleted. Reduction: `differs-checksum-L11-165-c0.rs`.

**20. AArch64, LLVM `-O2` with fast emit.** Three of the 24 programs of the first AArch64 run
(`A1-17`, `A1-21`, `A1-23`) died with SIGSEGV under `llvm`, and under no other backend; the
same programs are right on x86-64. The reduction of the first is 24 lines: a closure given to
`catch_unwind` that makes many vectors, each dropped on some paths only, and an `assert!`
that fails. The binary dies in `free` in the landing pad. What I measured: with
`-aarch64-enable-global-isel-at-O=-1` (or `-global-isel=0`) passed to LLVM the three programs
and the reduction are right, and `-fast-isel=0` changes nothing. What I infer from the
assembly, and did not confirm with LLVM's developers or a newer LLVM: fast emit selects
instructions at `-O0`, which on AArch64 means the global instruction selector and the fast
register allocator; after the `-O2` passes several drop flags are live in registers across
the call that unwinds, the selector's copy of the exception selector register is dead and
deleted while the register stays a live-in of the landing pad, and the allocator hands that
register to a drop flag without reloading it. The landing pad then reads the selector value
as a flag and frees a vector that was never made. blaze now turns the global selector off for
AArch64, so fast emit uses SelectionDAG's fast path there, as it does on x86-64. This is a
workaround, and what it costs in compile time on AArch64 was not measured. Reduction:
`differs-died-A1-17-c23.rs`.

**21. Finding 15 again, past an or-pattern.** `L12-18`, under all four backends. The match
was

    [Loud::C('A'..='Z', _), ..] => 1,
    [Loud::C('b', _) | Loud::I(..3, _), ..] => 2,
    [Loud::C('A'..='Z', _), m @ .., _] => 3,

and the third arm, which would move the middle out, can never be reached. rustc's tree has
no edge to it and drops the array front to back; blaze dropped it from the back. The
simulation of finding 15 took the first arm's range test, and sorted the first and third arm
into its branch, but the second arm starts with an or-pattern, which no test sorts, so the
third arm was held back behind it and tested again later, by itself. rustc does not do that:
`match_candidates_inner` in `rustc_mir_build/src/builder/matches/mod.rs` expands or-patterns
before it makes any test whenever any remaining candidate starts with one, not only the
first, so the second arm is two plain candidates by the time the range test is made and
the third arm goes with the first. The simulation's loop now has that order; the change is
four lines. The breaker case has three more functions, checked against rustc's MIR as
before: the match above, the same with the or-pattern arm first (not reached either), and
one where the arm in between has two or-patterns, which rustc expands only after the arms
before it are done, so there the third arm is reached and the array goes from the back.
Reduction: `differs-checksum-L12-18-c0.rs`.

`fuzz.py check fuzz/found/*.rs` with the binary of e45ffdff69: all 38 files give the same
output as rustc under both backends, with no function left to rustc
(`BLAZE_BIN=blaze python3 fuzz/fuzz.py check fuzz/found/*.rs`, 76 lines of `same`).

## Generated edits

`harness/fuzz/edits.py` is the second mode. A round is one generated program under a daemon
that keeps its binary up to date, the way `splices.py` runs one. Each step changes the source,
asks the daemon for the binary, which it splices, patches or links in full as it sees fit,
and builds the same source with cargo from nothing in another directory. A finding is a step
whose binary prints something else than cargo's, a step the daemon refused, a daemon that
died, or one that panicked and carried on. An edit that rustc turns down is not shown to the
daemon. The source of a case depends only on the seed, the case and the place the generator's
random stream is cut, so an edit can generate part of a case anew and leave the rest as it
was. The edits:

| Edit | What changes |
|---|---|
| `regen.run` | The body of a case's `run`, generated anew |
| `regen.fn` | The functions of a case from one on: bodies, signatures, more or fewer functions |
| `regen.constants` | The constants of a case and what uses them |
| `regen.adts` | The types of a case and everything after them |
| `const.value` | The value of one constant or static |
| `layout.fields` | The fields of a struct or the variants of an enum, in another order |
| `layout.repr` | `#[repr(C)]` put on a type or taken off |
| `body.literal` | One number in one function |
| `fn.add`, `fn.signature`, `fn.remove` | A function of the edit's own and its call, then its signature, then gone |
| `undo` | The source of an earlier step again |

`edits.py smoke` (2 programs, 8 steps each, both backends) is in the Linux CI job after the
fuzz smoke. `edits.py check` runs the sequences under `found/edits/` again.

The runs, each with `python3 fuzz/edits.py run --seed S --rounds N --cases 6 --steps 12
--backends cranelift,llvm`, counts per backend from `edits.py report DIR`:

| Run | blaze | Programs | Edits shown | Wrong output | Refused | Died | Spliced | Linked in full | No link needed |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| E1, 12 rounds of 16 steps | before the fixes | 12 | not kept | 0 | several | 1 | | | |
| E2, 16 rounds | ea6387473d less the third fix | 16 | 144 | 0 | 3 | 0 | 75 | 66 | 3 |
| E3, 24 rounds | 4602a6909f | 24 | 218 | 0 | 7 | 0 | 125 | 86 | 7 |
| E5, 24 rounds | e45ffdff69 | 24 | 235 | 4 (one cause, open, below) | 0 | 0 | 154 | 81 | 0 |

Up to E3 no run had a step whose binary printed the wrong thing: every finding was the daemon
saying no, or dying. Four causes, all fixed, each with a sequence under `found/edits/`:

- **The daemon died** (`listed-function-gone.rs`, `block-gone.rs`). After an edit that
  removed a function nested in a body, the daemon panicked with `Can't find ast id` in
  `span/src/ast_id.rs`: an outline of the file went through the functions listed before the
  edit, and one of them was no longer in the source. An outline now asks for the functions
  the file has now.
- **A refusal with `Duplicate definition of identifier`** (`block-namesake.rs`). An edit made
  a second block in a function with a nested function of the same name as the first block's.
  rustc tells them apart by a number blaze cannot know without rustc, and blaze gave both
  the first one's symbol.
- **The same through a type** (`derived-namesake.rs`). Two blocks of one function each with a
  type of the same name and a method or derive on it, three times in E2. Both are fixed by
  one rule: a path that two functions of a body share has no rustc symbol, and such
  functions get blaze's own.
- **A refusal with `unsupported: intrinsic powif32`** (`intrinsic-new.rs`), all seven refusals
  of E3. blaze did not know `powi`. A whole build seldom shows it, because blaze calls the copy of
  `f64::powi` that rustc's build of the crate has when there is one; an edit that brings the
  first `powi` into the program has no such copy to call. Whole builds showed it twice in
  the 57 programs of L12 (`L12-20`, `L12-46`), as a function left to rustc with that reason, so the program
  was still right; I take it rustc had inlined `powi` there and left no copy, and did not
  check; reduction `left-intrinsic-powif32-L12-20-c25.rs`. blaze now compiles the intrinsic (a call
  to compiler-builtins' `__powisf2`/`__powidf2`, which is what rustc's is on these targets).
  The same can be true of anything else in `std` that blaze cannot compile and takes from
  rustc's object, and only an edit shows it; whole-build runs count such a function as
  compiled.

E5, on the binary with `powi`, had no refusal and four steps per backend whose output
differed, in two programs (`E5-15` steps 9 and 10, `E5-21` steps 9 and 10), the same under
Cranelift and LLVM. This is **open and not looked into**; the work was stopped here. What
the output shows: the checksums are equal, and the difference is the file name in the
location of a panic raised inside `core` ("attempt to exponentiate with overflow" in
`core/src/num/mod.rs`). cargo's binary prints the path of the installed `rust-src`
(`~/.rustup/toolchains/.../library/core/src/num/mod.rs:1290:5`) and the daemon's binary
after the edit prints `/rustc/<commit>/library/core/src/num/mod.rs:1290:5`. My guess, not
checked: after the edit blaze compiled an instance of the integer `pow` itself and named
the file the way `core`'s metadata does, where rustc maps that path to the local sources
when they are installed. If so it is a difference in panic text only, it should also show
in a whole build on a machine with `rust-src` whenever blaze compiles a `core` function
with a location itself, and the fix is to apply rustc's mapping of `/rustc/<commit>` to the
sysroot's sources. The first sequence is kept as `found/edits-open/std-panic-path.rs` (294
lines, not reduced further; it is outside `found/edits/` so that `edits.py check` stays
green).

The refusals were correct behaviour in the sense of the rules (stated reason, the build falls
back to cargo), but each stood for an edit a user would have had to rebuild for. Why the
daemon linked in full instead of splicing, in E3 under Cranelift: 55 times the whole object
was written again, 6 times the patch had thread-local data, 5 times a function already
patched went back to what it first was, 11 times a symbol of libm or compiler-builtins was
not in the executable (`floor`, `fma`, `__rust_i128_mulo` and others), 6 times a function was
too short to jump from, and 3 times a static grew. None of those is wrong; they are what an edit costs.

## AArch64 under qemu

`BLAZE_TARGET=aarch64-unknown-linux-gnu python3 fuzz/fuzz.py ...` builds both sides for
AArch64 with the `aarch64-linux-gnu-gcc` linker and runs the binaries with `qemu-aarch64`, as
`buildmodel.py` does; blaze itself runs on x86-64 and cross-compiles. The smoke run there
(2 programs, 477 functions generated, 676 compiled under each backend) found nothing.

| Run | blaze | Programs | Functions generated | Compiled per backend | Cranelift | LLVM `-O2` fast | LLVM `-O0` | LLVM `-O2` full |
|---|---|---:|---:|---:|---|---|---|---|
| A1, 24 rounds | ea6387473d | 24 | 7,049 | 8,515 | 0 | 3 died (finding 20) | 0 | 0 |
| A2, 20 of 24 rounds (stopped) | 4602a6909f | 20 | 5,694 | 7,355 | 0 | 0 | 0 | 0 |

So the generator does find target-specific differences: three programs in 24 died on AArch64
under one LLVM configuration and are right on x86-64 under all four. All three are finding
20, and all three are right with the workaround (`fuzz.py repro A1-17`, `A1-21`, `A1-23`
with `--backends llvm`). Cranelift on AArch64 had no finding in these 24 programs, which at
7,000 functions is a small sample next to the x86-64 runs.

A2 ran on the binary with the workaround and without `powi`, and was stopped at 20
programs: no difference and no crash under any of the four, and under each of them three
functions left to rustc in two programs, all from `powi` (`A2-10`: `intrinsic powif32`;
`A2-17`: a `thread_local!` in a function left to rustc for `powi`, reported as `writable
static without a rustc symbol`, and the `async` block that shares it). The reduction of
`A2-17` is right for AArch64 with the `powi` binary; `A2-10` was not run again. So the
only target-specific finding in 44 AArch64 programs is finding 20.

## The merge

`blaze-collate` moved to ce52276cf5 while this was running, and I merged it in (27920a4f69).
It conflicted where it mattered: the same weeks' work on rustc's own test suites had fixed
drop order in the same functions, differently. Both sides had a fix for the temporaries of an
`if let` before 2024, for `let ... else`, and for the order an array is dropped in after a
pattern gave its elements back. What I kept:

- From this branch, in `mir/lower.rs`: an `if let` that goes straight from the pattern to its
  branches, a scope of its own for the temporaries of each branch of an `if`, and its
  `lower_let_else`. The other side's `end_of_branch`, `forget_branch`, `outlive_if_let` and
  `lower_let_else` are gone; its breaker cases for them (`if_let_outlives`, `let_else_drops`,
  `tail_tuple_drops`, `guard_let_drops`, `repeat_moved`) pass with this branch's code.
- From `blaze-collate`, in `mir/codegen.rs`: the analysis of which fields may be moved out
  where a drop is (`moved_where_blocks_end`), the stack argument modes (which include the fix
  of finding 5), and scope ends as the place of drops. This branch's
  `MirBody::parted_at_block_ends` answered nearly the same question and was still there
  beside it after the merge. It is gone since 20d97d4d73: the other analysis alone passes
  every breaker case, including the ones written for this one.
- The edition 2021 breakers are one crate, `breakers2021`, with both sides' cases.
- `OPEN` in `breakers.py` is both sides' open cases.

Measured on the merged binary: the required harnesses below, `fuzz.py check` (29 of the 30
files the same, the one of finding 15 differs, as before the merge), the smoke run (no
finding), and one more run, shorter than the others: `--seed L10 --rounds 60`, 60 programs,
none rejected, 1,800 cases, 17,297 functions generated, 16,734 compiled by blaze under each
backend, no difference, no crash, nothing left to rustc. So the 100,000 functions per backend
without a finding were run on 27a94f2770, the head before the merge, and the merged head has
the gate, the reductions and 17,000 functions behind it.

## The required harnesses

Run from `harness/` with `BLAZE_BIN` at the release binary of the merged head (27920a4f69), once with
`BLAZE_BACKEND=cranelift` and once with `BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`. The
results were the same under both:

| Command | Result |
|---|---|
| `python3 difftest.py corpus --bin runner --crate cases --layouts` | exit 0, 0 of 893 layouts differ |
| `python3 programs.py` | 22 of 22 right |
| `python3 stress.py` | 99 of 99 right, 0 restarts |
| `python3 breakers.py` | 67 of 67 right, 6 open, 0 functions skipped |
| `python3 unwinds.py` | 8 of 8 right |
| `python3 splices.py` | 71 of 71 right |
| `python3 asmedits.py` | 10 of 10 right |
| `python3 includes.py` | 9 of 9 right |
| `python3 worktrees.py` | 62 of 62 right |

The table is as of e45ffdff69 (finding 21 fixed, `blaze-collate` 3d47ed59bf merged), run
again under both backends; on the first merge commit the breakers were 58 of 58 right with
7 open, the rest the same. The 6
open of `breakers.py` are finding 3 of this report, three that
`blaze-collate` lists as open and two it lists as left to rustc. Before the merge, on
27a94f2770, the same nine passed under both backends with 40 of 40 breakers right and 2 open.

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser -p
syntax -p blaze-engine` on e45ffdff69: 513, 40, 330, 71 and 56 tests passed in the test binaries
that have tests, 0 failed, 1 ignored.

The other harnesses `blaze-collate` has in CI now (`keys.py`, `backends.py`, `debuginfo.py`,
`buildmodel.py`, the `store*.py` ones, `remote.py`) are not in the required list and I did
not run them on the merged head.

## Open, and not done

Stopped mid-way when the work was wound down; in order of what I would do next:

- The difference E5 found in generated edits (the path in a panic location inside `core`,
  see "Generated edits") is open and its cause is a guess.
- The four-backend long run on the head is 60 programs (L13), not 360.
- `blaze-collate-next` is not merged into this branch; `blaze-collate` at 3d47ed59bf is.
  The gate was last run on e45ffdff69 and passed under both backends.
- A2 was not run again on the head, and no AArch64 run has the `powi` fix.
- There is no table of how often each scene was generated in a run; `fuzz.py run
  --coverage` prints it.

- Finding 3, the `assert!` message, is open for the reason above.
- Finding 6 no longer shows with the SLP vectorizer on (2026-10-23), with no change to LLVM:
  what in blaze's IR it depended on is not known, and AArch64 has not had a long run with it.
- Finding 20 is a workaround too, and its diagnosis is mine from the assembly; nobody who
  knows LLVM's fast register allocator has looked at it. Its cost on AArch64 was not measured.
- macOS and Windows were not run. The ABI classes the generator aims at are the System V and
  AAPCS64 ones.
- The scenes are templates with random values and order, not free-form generation: a scene
  finds what its template can show. Coroutines other than `async` are not stable and are not
  generated. Inline assembly is one x86-64 scene and one AArch64 scene of register arithmetic.
- The generated edits change one file of one crate. They do not add or remove files, change
  `Cargo.toml`, or edit a dependency.
- The generator's programs are single-crate and use only `std`.
- A reduction is bounded by time (`--reduce`, 600 s in the long runs), so a reduced file is
  small but not always minimal. The breaker cases were written by hand from them.
- The CI jobs for Windows, macOS and AArch64 do not run the fuzzer; the AArch64 runs here were by hand.
