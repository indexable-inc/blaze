---
status: current
summary: The differential fuzzer: how it makes programs, runs them under cargo and blaze, and what it does not cover.
---
# The fuzzer

Every other harness runs programs somebody wrote. The fuzzer makes programs up, builds each
with cargo and with blaze, and compares what they print. It found twenty-one bugs the other
harnesses had not, most of them miscompiles, so it is worth knowing how to run it and what
it does not cover. The full account, with every finding, is in
[reports/FUZZ.md](reports/FUZZ.md).

## Running it

    cd harness
    export BLAZE_BIN=$PWD/../target/release/blaze-engine
    python3 fuzz/fuzz.py smoke                                  # 2 programs, fixed seed; in CI
    python3 fuzz/store.py                                       # asm changes, cold and warm store; in CI
    python3 fuzz/fuzz.py run --seed S --rounds N --jobs 8 --out DIR
    python3 fuzz/fuzz.py repro S-ROUND [CASE ...]               # one program again, or some of its cases
    python3 fuzz/fuzz.py check fuzz/found/*.rs                  # every stored reduction against this blaze
    python3 fuzz/fuzz.py report DIR [--coverage]                # add up the logs of a run

By default each program is built under both backends, so `BLAZE_BIN` should be a build with
`--features llvm`; `--backends cranelift` runs one. LLVM is run with
`BLAZE_OPT=2 BLAZE_EMIT=fast`. The exit status is 1 when there is a finding.

`store.py` uses fresh stores of its own. It compiles a function, changes only its `asm!`
text, then builds each version again from the store, comparing every output with rustc.
It also runs the smoke seeds twice against another store, empty on the first pass and warm
on the second. This covers #418, where the uncached smoke run passed but smoke-1/c16 read
another assembly wrapper's code from the LLVM store. The assembly is part of the cache key
since `2a3a0a35`; `fuzz/found/differs-checksum-smoke-1-c16.rs` records the recovered reduction.
On failure the script prints and keeps its work directory, including each smoke pass's logs.

`run` also takes `--cases` (cases per program, 30 by default), `--first` (the number of the
first round), `--reduce` (seconds a reduction may take, 0 to only count) and `--work`
(where the programs are built).

A round is one program of about 290 functions. It costs one cargo build and one blaze build
per backend. The smoke run took 31 s under Cranelift and 53 s under LLVM on the machine
these documents were checked on. A full-size run of 360 programs is long; use `--jobs`.

## What a program is

`fuzz/gen.py` writes a program from a seed: `fuzz/prelude.rs` followed by one module per
case. A case is a set of types, traits and helper functions and a `run` function. Everything
a case computes is fed into one 64-bit checksum. `main` runs each case inside
`catch_unwind` and prints one line per case with its name, its checksum and whether it
ended or panicked. Panic messages are printed too, so the place a panic is reported at is
compared as well.

The source of case N of seed S depends only on S and N. A case can therefore be made again
on its own, and round N of a seed is the same program whatever `--jobs` is.

The generator has to produce programs on which two correct compilers cannot disagree:

- Arithmetic is wrapping, checked, saturating or overflowing. Plain `+`, `/`, `<<`,
  indexing and `unwrap` appear only where the panic is caught and it is known whether they
  panic.
- No address, NaN bit pattern, sign of a zero or hash order is observed. Maps are
  `BTreeMap`.
- Raw pointers stay inside the object they came from and read only what was written.
  Unions and `MaybeUninit` are read only through the field that was written.
- Every loop has a counter the generated code cannot assign, calls go only to functions
  defined earlier, and recursion counts down.
- Half the programs are built as edition 2021 and half as 2024, because drop order differs
  between them.

The generator also combines scenes for async futures, `dyn Any`, inline assembly and
unwinding (`fuzz/scenes.py`). It does not generate threads, macros of its own, SIMD
intrinsics, or any I/O other than the checksum lines. Programs are a single crate and use
only `std`.

Every case also has a few `const fn`s (`fuzz/constfn.py`), each with a constant that calls
it. `run` starts by calling each one again on the same arguments through `black_box`, and
both results go into the checksum. blaze's build agrees with cargo's only when blaze's
interpreter, which evaluates the constant, and its compiled code agree with rustc. A body
uses what a `const fn` may use without a feature: integer and float arithmetic (float `%`
too), `as`, `while` and `loop`, `match`, arrays, `&mut` to its locals, calls to the const
fns before it, and a const generic length. A NaN goes into the checksum as one value.

## What counts as a finding

- A case whose line differs from cargo's: the checksum, the panic location, the panic
  message, whether it panics, or the program dying.
- A function blaze's report lists as left to rustc.
- A blaze build that did not finish.

Each finding is reduced in two steps, first over the cases of the program and then over the
lines of the one case left, keeping a candidate only while rustc still accepts it and the
same kind of finding still shows. The result is written to
`fuzz/found/<kind>-<key>-<seed>-<case>.rs` with a header naming the seed, the edition, the
backend and both outputs. A cause already seen in the same run is counted and not reduced
again. Reduction is bounded by time, so a reduced file is small but not always minimal.

## From a finding to a fix

1. `fuzz.py repro SEED-ROUND CASE` to see it again.
2. Read the file under `fuzz/found/`. Reduce it further by hand to a function that shows
   the cause.
3. Add that as a breaker with the cause written at the top
   ([harness.md](harness.md), "Adding a breaker"). The breaker is the regression test; the
   file under `found/` is the evidence.
4. Fix it, run `fuzz.py check fuzz/found/*.rs` and the gate.
5. Run a new seed. Several times, a run that was otherwise clean found one to three more
   bugs of the same family as the one just fixed.

## What it has found

Twenty-one distinct problems in whole builds, on Linux x86-64 with Rust 1.99.0, one of them
only on AArch64. Most were miscompiles: the program ran and did the wrong thing. Nearly all
of those were the order in which temporaries and partly moved values are dropped, in
`if let`, `let ... else`, `match` guards, `for` loops in tail position and when an operand
panics. One was an index that was not bounds-checked when its value was unused. The rest
were wrong panic text and functions blaze refused.

Eighteen are fixed. Two are worked around and look like LLVM bugs rather than blaze's: an
array of `i128` rows left partly unwritten, which blaze avoids by turning a vectoriser pass
off, and a crash while unwinding on AArch64 at `-O2`. One is open and listed in
[support.md](support.md): the text of a long `assert!` condition.

Generated edits against a running daemon found four more problems in the incremental path,
all fixed, and a fifth that is open and has not been looked into: after some edits a panic
inside `core` names the file by its `/rustc/<commit>/library/...` path instead of the local
one.

The long runs are tabulated in [results.md](results.md). Almost every full-size run found
one to three more drop-order bugs, which were fixed before the next. The last two runs were
stopped early, at 57 programs (finding 21) and at 60 programs with nothing found. Expect
that more exist.

## What it does not cover

- Generated edits have had a few short runs only (24 rounds in the last), so the daemon's
  incremental path, the patch object and the splice are barely fuzzed.
- Only Linux x86-64 has been run. The ABI shapes the generator aims at are the System V
  ones. The smoke run is in the Linux x86-64 CI jobs only.
- Single-crate programs using only `std`: nothing about dependencies, features, build
  scripts or proc macros.
- The constructs listed above that the generator does not make.
