---
status: record
summary: Debug info: DWARF for ELF, and the Mach-O design.
date: 2026-10-20
---
# Debug info

Branch `blaze-debuginfo`. Everything of mine here is Linux x86-64 ELF. Mach-O is a design
note at the end; Windows has lines and procedures in CodeView from the Windows work, which
came in with the merge of `blaze-collate-next` (WINDOWS.md). Rounds one and two (line tables, backtraces, the splice path, variables and
types under both backends, profile settings) are in `blaze-collate`. Round three is on top of
`blaze-collate-next` at 1b022ff6f2 and is described in "Round three" below; the sections marked
"round two" are kept as written then, and where they say something else about scopes,
closures, split debug info, the LLVM counts or cost, "Round three" is right.

## What there is now

Every function blaze compiles is described in DWARF 4: a `DW_TAG_subprogram` per function
with its name, linkage name, declaration file and line and its range of code, and a
`.debug_line` program that gives a file, line and column to every instruction that came from
a statement. The producer string starts with `blaze`, which is how the harness tells blaze's
units from the ones rustc wrote for the dependencies.

Round two: an object now has one compilation unit per 256 functions instead of one for the
whole object. With a single unit of 11,647 functions GNU `addr2line` took 1.1 s for its first
lookup in the corpus binary (15 ms in the cargo build), because it reads a unit's whole line
program to answer for one address. `perf` gives `addr2line` one second and then gives up, so
under LLVM perf lost the lines of some frames. With units of 256 the lookups are quick and
perf has a line for every frame again.

The two backends get there differently.

Cranelift: each MIR statement and terminator sets a `SourceLoc` on the instructions built for
it. The location is an index into a table of spans kept per function, not a line, so an edit
above a function does not change its compiled code or its cache entry. After emission the
source-location ranges of the machine buffer become rows of (code offset, span index). Lines
are looked up from spans only when the function is put into an object, and gimli's writer
produces `.debug_info`, `.debug_abbrev`, `.debug_line`, `.debug_str`, `.debug_line_str` and
`.debug_ranges` with their relocations (`crates/hir-ty/src/mir/codegen/debuginfo.rs`).

LLVM: the lowering from Cranelift IR to LLVM IR creates a `DISubprogram` per function through
LLVM's DIBuilder and puts a debug location on every instruction, with the same span index in
the place of the line number. LLVM writes a `.debug_line` for its module, blaze reads it back
with gimli into the same (offset, span) rows, and from there the path is the one Cranelift
uses. So the optimizer decides which instruction belongs to which statement, and the final
sections are written by the same code for both backends.

Names are what rustc's demangler prints for its v0 symbols: `krate::module::f::<u8>`,
`<T as Trait>::f`, `f::{closure#0}`, with default generic arguments left out and closures
numbered in rustc's order.

The end of a scope, where its drops run, is given the place of the closing brace, as rustc
does. Before this the drops had the line where the scope's expression begins.

Debug info follows the profile; see "Profile settings" below.

## Round three: scopes, richer types, split DWARF, real projects

### At this head

This head has `blaze-collate-next` 1b022ff6f2 merged in (a plain merge). The gate below was
run once on it, on Linux x86-64, under both backends. What `debuginfo.py` printed:

| part | Cranelift | LLVM -O2 |
|---|---|---|
| corpus backtraces | 85 panics, 448 frames, same | 447 frames; 1 function seen to leave by a jump (see "Tail calls") |
| callers | 6 panics, 19 frames, same | 18 frames, 1 function seen to leave by a jump |
| `llvm-dwarfdump --verify` | no errors in any binary | no errors in any binary |
| `.eh_frame` and ranges, corpus | 11,700 functions, 0 disagree | 0 disagree |
| a line for every instruction | 0 bytes without, 11,700 functions | 0 bytes without |
| locals (gdb) | 9 of 9 | 8 of 9, 1 optimized out that rustc keeps |
| types (`rust-gdb`, 21 values) | 21 of 21 | 21 of 21 |
| scopes (30 names by block) | 30 of 30 | 23 of 30; 7 optimized out, 2 of them kept by rustc |
| rich (`rust-gdb` and `rust-lldb`, 15 values) | 15 of 15 in both | 14 of 15 in both; the one is lost by rustc too |
| profile, 11 settings | all followed | all followed |
| edits, and again with `split-debuginfo = "unpacked"` | 6 of 6 spliced, frames same, in both | 6 of 6 spliced, frames same, in both |
| last line | `all right (cranelift)` | `all right, 2 open (llvm)` |

The gate: difftest MATCH with 0 of 893 layouts differing, programs 22 of 22, stress 99 of 99,
breakers 94 of 94 with 6 open, unwinds 8 of 8, splices 71 of 71, asmedits 10 of 10, includes
9 of 9, worktrees 62 of 62, keys 46 of 46 with 0 open, buildmodel 194 of 194 as expected, the
five store harnesses (`storekills`, `storecorrupts`, `storefull`, `storeshared`,
`storeguards`) pass, `selects.py` 3 of 3, `fuzz/fuzz.py smoke` and `fuzz/edits.py smoke`
exit 0, `backends.py` under LLVM exits 0, `blaze_cli.py --small` PASS under both; `cargo fmt` clean, `cargo test`
1015 passed, 0 failed, 1 ignored. `realdebug.py` (ripgrep and axum) was last run on the
head before this merge and not on this one.

`blaze_cli.py` did not pass where the gate first ran it and passed when run again alone,
under both backends, for reasons of this machine: a scratch `TMPDIR` long enough to put
the daemon's socket past the length a socket path may have, and then a full disk.

### What the merge of 1b022ff6f2 needed

`blaze-collate-next` had made `DebugContext` an enum of two writers, DWARF for ELF and
CodeView for COFF, where this branch had one DWARF writer that had grown since (indexed
types, scopes, units of 256 functions, split DWARF). The writer of this branch is now the
`Dwarf` of that enum, unchanged inside; `emit` returns the files kept apart, which CodeView
has none of. The timing steps of the perf work (`Tally`, `Step::Describe`,
`Step::StoreWrite`) are around the same calls as there. In `harness/debuginfo.py` the
Windows paths (`Pdb`, `Image`, `cdb`, `.pdata` against the procedures) are kept beside the
ELF ones; the parts that ask gdb, lldb and `llvm-dwarfdump` for what only DWARF has so far
(`callers`, `types`, `scopes`, `rich`, `profile`, and the second run of `edits` kept apart)
say `not run` on Windows. That Windows side of the script was merged by reading and was not
run: there was no Windows machine in this session.

The script in `blaze-collate-next` before this merge is the one of round one. The row that
failed on the AArch64 runner is explained below and is fixed by this branch's script. Of the
one failure under LLVM in the coordinator's gate I do not know the row; the script that
failed is replaced whole by this one, which passed under LLVM each time it was run here.

### What the merge of 14ee89181b needed

`blaze-collate` had come to guard the reading of a stored entry against debug types nested
deeper than a stack can follow, on the types as round two had them, each holding the ones
inside it. Round three keeps a function's types in one list and names them by index, so
reading them cannot recurse and that guard is not needed there; the depth is checked
instead where the entries are written (`DebugContext::ty`, at most `DebugType::DEEPEST`
inside each other, the rest left out), and the lexical blocks of a function are now built
without recursion and a scope that is inside itself is left out. The test in `cached.rs`
reads back an entry with types 40,000 deep, and writes the tables for it, for a scope that
is its own parent and for scopes 40,000 deep, and sees each end. In `cargo/state.rs` the
packed debug info (`.dwp`) is installed beside the program where `blaze-collate`'s new code
installs the program; no harness builds `packed` through `blaze`, so that path is
compiled and not measured.

### The AArch64 runner's `wrong: 1`

In CI on `blaze-collate` 14ee89181b (run 39086041463, `harness (cranelift, aarch64)`),
`debuginfo.py` failed on one row: `corpus: .eh_frame and the ranges  DISAGREE  11735
functions, 1 without an entry, ... 5 of assembly alone`. It is the harness and not the debug
info, and it is the miscount this branch had already fixed for x86-64: a `#[naked]`
function has no unwinding entry and no line, and the script took the row that ends the line
sequence of the function before it, which is at the address the naked one begins at, for a
line of the naked one. `blaze-collate` has the script from before that fix. Measured here by
building the corpus for `aarch64-unknown-linux-gnu` with Cranelift on the x86-64 host
(`BLAZE_TARGET=aarch64-unknown-linux-gnu python3 debuginfo.py --only corpus`) and running
this branch's script on it: `11735 functions, 0 without an entry, ... 6 of assembly alone`,
the same 11,735 functions and the same six. It was not run on the runner itself; the rows of
that cross run that execute the program (panics, gdb) do not apply under qemu and are not
counted.

Refused, with a reason, when the build is planned: a `debug`, `strip` or `split-debuginfo`
value blaze does not know. A splice into an executable with compressed debug sections is
refused and blaze links in full. Nothing else in debug info is refused; `packed` and
`unpacked` were until this round.

Merging `blaze-collate` 8f21942505 changed where an out-of-bounds index is reported, and the
line row of the check no longer followed it, so a backtrace had the line of the statement
where the message had the line of the index expression. The row is now set with the place the
message uses.

### Lexical scopes

A function's variables are now inside `DW_TAG_lexical_block` entries that follow the blocks
of the source, so a local shows in the debugger only where it is in scope, and a name bound
more than once (shadowing, a `match` arm that binds the name of an outer local) has one
entry per binding. The ranges come from the MIR: every statement knows the expression scope
it was lowered from, and the line rows of a function give the code of each scope. A binding
of a `match` arm belongs to the scope of the arm.

`debuginfo.py --only scopes` stops at five lines of a function that binds `value` five
times and asks the debugger for every name at every stop, 30 questions, and compares with
the cargo build, including the names that must not be known at a line. Cranelift: 30 of 30.
LLVM -O2: 23 of 30 with 7 `<optimized out>`, none wrong; see "LLVM and optimized-out values".

### Closures, coroutines, `dyn`, enums

- A closure is a struct whose fields are its captures by name, and the closure's body sees
  the captures as variables.
- An `async fn` or `async` block is an enum with the variants rustc gives it: `Unresumed`,
  `Returned`, `Panicked` and one `SuspendN` per await point, each with the locals that live
  across that point; the future being awaited is `__awaitee`. Locals of an async body that
  live in the future are described through the pointer to it.
- `&dyn Trait` and `Box<dyn Trait>` are a pair of `pointer` and `vtable`. A pointer to a
  struct that ends in an unsized field (`&Path`) is a fat pointer with its length.
- Enums have a `DW_TAG_variant_part` with the discriminant and a member per variant, in
  the shape rustc writes, for tagged enums and for niche ones (`Option<&T>`).
- Functions and types are nested in `DW_TAG_namespace` entries for their modules, so a
  debugger prints `krate::module::Type` and can find `krate::module::function`.
- Closures and async blocks are named as rustc names them for a debugger
  (`main::{closure#0}`, `{async_block#0}`, environments as `{closure_env#0}`).

`debuginfo.py --only rich` prints 15 such values in `rust-gdb` and in `rust-lldb` and
compares them with what each prints for the cargo build. Cranelift: 15 of 15 in both.
LLVM -O2: 14 of 15 in both; the one missing, `object` (a `&dyn` argument), is optimized out
in `rustc -O -g` as well.

### LLVM and optimized-out values

Before the default passes blaze now runs `sroa,instsimplify,sroa` on each function, so that
a stack slot the optimizer takes apart is described piece by piece (`DW_OP_piece`) instead
of being dropped; constants read from read-only data are given to LLVM as constants. With
that the `types` part is 21 of 21 under LLVM (was 20) and `locals` is 8 of 9 (was 6).

The harness now holds every value LLVM reports as optimized out against a `rustc -O -g`
build of the same source at the same line (`cargo build --release` with `debug = true` and
the same opt level). A value rustc does not show either is accepted. A value rustc shows
makes the row `behind rustc`, which is counted as open in the last line of the script
(`all right, 2 open (llvm)`) and does not fail it; a wrong value still fails.

What is open by that measure, under LLVM -O2:

| part | value | why blaze loses it |
|---|---|---|
| locals | `width` at `main.rs:11` | an argument that is dead after its one use. rustc inlines `area` into `main`, where `width` is the constant 10. blaze compiles `area` alone and its register is reused. |
| scopes | `step` at `main.rs:16` | the loop variable is the second half of what `Range::next` returns and is never read. rustc inlines `next` and knows the value; blaze calls it, and LLVM drops the unused half of the result. |
| scopes | `value` at `main.rs:16` | `[step, total]`: the `total` half is described (`DW_OP_piece`), the `step` half is the value above, so gdb shows the array as partly optimized out. |

All three have one cause: blaze compiles every function in a module of its own and does not
inline, so a value that rustc's inlining turns into a constant or a known register is, for
blaze's LLVM, the dead result of a call. I tried keeping named locals of up to 16 bytes
alive to the end of the function with `llvm.fake.use`; the values stayed alive but LLVM
still marked them unknown inside the loop, and one more value was lost, so it is not in the
tree. The other five optimized-out answers in `scopes` and the one in `rich` are lost by
`rustc -O -g` too. Under Cranelift nothing is optimized out.

### Split DWARF

`split-debuginfo = "unpacked"` and `"packed"` are now done rather than refused.

- `unpacked`: each unit of blaze's is a skeleton in the object (`DW_AT_GNU_dwo_name`,
  `DW_AT_GNU_dwo_id`, the ranges and the address table) and a `.dwo` file beside the object,
  named `<object>.<n>.dwo`, with the `.debug_*.dwo` sections. This is the GNU extension to
  DWARF 4, which is what rustc writes at this version.
- `packed`: the same, and after the link a `.dwp` is written beside the executable from the
  `.dwo` files and the package rustc wrote for the program it linked, with the `thorin-dwp`
  library (the one rustc uses).
- `off` or unset: everything in the object and the executable, as before.

`debuginfo.py --only profile` builds the locals program under each setting and checks that
the units are kept apart, that the files exist, that gdb prints the same nine values as for
the linked build, and that `llvm-dwarfdump --verify` has no error.

Limits: the split writer writes little-endian sections and has only been run on Linux
x86-64. `lldb` 20 reports the ranges of a block in a split DWARF 4 unit as invalid; it does
the same for a rustc build with `split-debuginfo` of more than one unit, and the script
counts those messages instead of failing on them.

A splice into a program built with `unpacked` is checked by `debuginfo.py --only edits`,
which now runs its six edits a second time with the profile set so (`edits, kept apart`).
After the last splice the units the linker placed are still skeletons with their `.dwo`
files, the unit of the spliced code is whole in the appended sections, the backtraces are
those of a fresh cargo build, every instruction has a line, and gdb stops at a line of the
edited program and prints an argument there as it does for the cargo build (under LLVM
the argument is `<optimized out>`, which the row lets pass, so there it shows only that
gdb stops). All six edits are spliced under both backends. A splice into a
`packed` program has not been tried; the `.dwp` is written at the link and a splice does
not write it again, which should not matter since spliced units are not split, but that is
inferred and not measured.

### Real projects

`harness/realdebug.py` clones ripgrep (15.1.0) and axum (`axum-v0.8.6`, the
`example-hello-world` server), puts one panic into each, builds with cargo and with
`blaze` in the dev profile, and compares:

    cd harness
    BLAZE_BIN=$PWD/../target/release/blaze-engine python3 realdebug.py ripgrep axum

| | ripgrep | axum hello-world |
|---|---|---|
| `llvm-dwarfdump --verify`, blaze's units | 91 units, 0 errors | 31 units, 0 errors |
| gdb: stop at each place, name the same arguments and locals as the cargo build, same plain values | 6 places, all as cargo's | 6 places, all as cargo's |
| panic with `RUST_BACKTRACE=1`, frames of the workspace by file:line | 5 frames, same | 6 frames, same |
| `perf record --call-graph dwarf`, `perf report` | 151 symbols, 58 of blaze's functions, 0 without a name | 128 symbols, 74 of blaze's, 0 without a name |
| `perf script` lines | 163 frames in blaze's functions, 0 without a line | 80 frames, 0 without a line |
| `addr2line` at the start of every blaze function | 23,255 functions, 0 without a name, 0 without a line | 7,696 functions, 0 and 0 |

These are Cranelift numbers, taken at this head.

Under LLVM (`BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`), at this head:

| | ripgrep | axum hello-world |
|---|---|---|
| `llvm-dwarfdump --verify`, blaze's units | 91 units, 0 errors | 30 units, 0 errors |
| gdb places | 5 of 6 as cargo's; at one, `let mut searched = false;`, the optimizer left the line no code | 6 places, all as cargo's; 2 values optimized out |
| panic frames | 5, same | 6, same |
| `perf report` | 94 symbols, 10 of blaze's functions, 0 without a name | 59 symbols, 15 of blaze's, 0 without a name |
| `perf script` lines | 88 frames in blaze's functions, 0 without a line | 33 frames, 0 without a line |
| `addr2line` | 23,260 functions, 0 and 0 | 7,581 functions, 0 and 0 |

A place where optimized code has no instruction is reported as `no code` and passes only
under LLVM and only if gdb itself says the line "contains no code". The axum places were
five, two of which gdb did not stop at in the cargo build either, so there was nothing to
compare with; they are now six, each a statement gdb does stop at in the cargo build: two
in `main`, the handler's panic, two in `axum::serve` and one in the router. The script is not part of the gate or of CI, since it
clones from GitHub.

Getting there fixed things the small fixtures had not shown: fat pointers to structs with an
unsized tail were opaque; functions were not nested under their module, so gdb printed bare
names; compiler-made locals had names; rows of code written inside a macro inside a closure
pointed at the enclosing function; the code that drops everything when a future is dropped
half-way had the line of the `async` block and was laid out first, so a breakpoint on the
handler's line stopped in the drop path (that code, and the blocks that panic when a
finished future is polled again, are now cold and laid out behind the body); and perf could not unwind through spliced
code, because the segment a splice appends was not as far from the unwind table in the file
as in memory. `Image::behind` in `crates/blaze-splice/src/lib.rs` now places it so.

### A line for every instruction

`debuginfo.py` walks every function of blaze's against `.debug_line` in every binary it
builds and fails if any byte of one has no row (`a line for every instruction`). Before the
fix 1,056 of 11,647 corpus functions had no row at all: drop glue, `call_once` and reify
shims, and functions with no statement of their own such as the outer half of an
`async fn`. Each now has one row for where it is written, as rustc has it. `#[naked]`
functions have none, as under `rustc -g`.

### Cost

`debuginfo.py --only cost`: compile and link of the corpus `cases` object, best of three,
with `BLAZE_DEBUGINFO=0` and without.

| | Cranelift | LLVM -O2 |
|---|---:|---:|
| Compile + link, no debug | 7.74 s | 7.71 s |
| Compile + link, debug | 9.00 s (+16.3%) | 8.59 s (+11.4%) |
| Object, no debug | 41,497,184 | 37,074,488 |
| Object, debug | 50,463,088 (+21.6%) | 47,015,240 (+26.8%) |
| Sections a debugger reads | 4,649,872 | 4,821,124 |
| Their relocations | 4,316,032 | 5,119,628 |
| Binary, no debug | 86,995,944 | 83,613,408 |
| Binary, debug | 91,645,816 | 88,434,528 |

Measured at the head before the merge of `blaze-collate-next`. With it merged, the one run
inside the whole `debuginfo.py` gave 6.09 s and 7.42 s (+21.9%, object +21.6%) for
Cranelift and 6.99 s and 7.87 s (+12.5%, object +26.8%) for LLVM. The sizes are exact; the
time is not. Each backend was measured
twice here, once inside the whole `debuginfo.py` run and once alone: Cranelift gave 9.32 s
and 12.13 s (+30.1%) and then the row above; LLVM gave the row above and then 9.39 s and
9.66 s (+2.8%). Before the merge of 14ee89181b the same measure gave 6.12 s and 6.92 s
(+13.1%) for Cranelift and 6.53 s and 7.43 s (+13.8%) for LLVM, and over five earlier runs
between +13% and +19%. So the build without debug info takes a second or more longer on
this machine than it did a day ago, with or without the merge I cannot say, and the share
debug info adds is somewhere between a tenth and a fifth; these runs do not narrow it.

Round two had +13.4% and +10.4% in time and +16.0% and +20.4% in object size. The growth
since is the lexical blocks, the captures and coroutine variants, the namespaces, and the
one row for each function that had none. I have not profiled this round's additions; the
relocations of the debug sections are now as large as the sections themselves, and
addressing a unit relative to one symbol (DWARF 5 `addrx`) is the first thing to try.

## Variables and types (round two)

Both backends now describe parameters and `let` bindings, and they describe them from the
same data. A function's code carries, next to its line rows, a list of the stack slots that
hold named bindings and where each slot is: a `DW_AT_frame_base` expression and, per slot,
either an offset from the frame base or a list of (range of code, location expression).

Cranelift: a binding that lives in one stack slot is at a fixed offset from `rbp`.

LLVM: the lowering gives every such slot an `alloca` and a `dbg.declare` through DIBuilder,
under a made-up variable named after the slot number and with a made-up integer type of the
slot's size. LLVM then does what it does for clang: where the optimizer keeps the slot in
memory it writes a frame offset, where it moves the value into registers or drops it, it
writes a location list or nothing. blaze reads LLVM's `.debug_info` back (frame base, each
slot's `DW_AT_location`, location lists resolved to ranges of the function) and throws LLVM's
unit away. The real names, types and scopes are then written by the same writer as for
Cranelift. So LLVM decides where a value is and whether it still exists, and blaze never
guesses: a slot LLVM says nothing about is described with no location, and gdb prints
`<optimized out>` for it.

A parameter passed by reference to a caller's copy (a large array or struct by value) is
described as the pointer's slot dereferenced.

Types. A function's variables refer to a table of types that is independent of DWARF:
base types, pointers, arrays, and aggregates with fields at their layout offsets. What is
described in full:

- integers, floats, `bool`, `char`
- references and raw pointers; `&str` and `&[T]` as `data_ptr` and `length`, which is the
  shape rustc writes and gdb's Rust support understands
- arrays, tuples, structs, tuple structs, unions
- enums: fieldless ones as `DW_TAG_enumeration_type` with their discriminant values; the rest
  as a `DW_TAG_variant_part` with the discriminant member and one `DW_TAG_variant` per
  variant, for both tagged and niche layouts (`Option<&T>` is a niche: the `None` variant is
  the default variant, as rustc writes it)
- `Option`, `Result`, `Box`, `Vec`, `String` and the other library types follow from the above:
  they are described field by field with rustc's type names (`alloc::vec::Vec<i32,
  alloc::alloc::Global>`) and with their generic parameters as
  `DW_TAG_template_type_parameter`, which is what the pretty-printers the toolchain ships
  (`rust-gdb`) look for. The harness compares what `rust-gdb` prints for a `Vec`, a `String`
  and a slice of the `Vec` with what it prints for the cargo build.

Closures, coroutines and `dyn` pointers were name and size only in round two; see "Round
three".

With `debug = "line-tables-only"` (or `1`, `limited`) the variables and types are left out
and only the line tables and subprograms are written.

## Tail calls, inlining and `#[track_caller]` (round two)

The frame that was missing under LLVM is a tail call, and I left it as it is. In the corpus
the panic at `cases/src/stabilized.rs:229` goes through

    fn or_die(v: Option<u32>) -> u32 {
        v.unwrap_or_else(|| diverge(77))
    }

and LLVM at -O2 compiles `or_die` to two instructions, `lea` of the closure and
`jmp unwrap_or_else`. There is no return address of `or_die` on the stack when the closure
panics, so no unwinder can show the frame, with any debug info. rustc does the same when it
optimizes: a three-function copy of this shape with `#[inline(never)]`, built with
`rustc -O -g`, prints `diverge`, `or_else`, `main` and no `or_die`. The frame is in the cargo
build only because that is a dev build. Turning tail
calls off for LLVM (`disable-tail-calls`) would bring the frame back and cost what the
optimization saves in every build, so the harness proves the explanation instead of the code
changing: for every frame that is in the cargo build's backtrace and not in blaze's, it finds
the function's range in blaze's debug info, disassembles it, and requires a jump out of the
function. A function that is missing and has no such jump is reported as `FRAMES LOST` and
fails the run. Under Cranelift nothing may be missing at all.

Inlining. Both backends compile one function per module (`llvm::emit` creates a module with
the one function), so no blaze function is inlined into another and there are no
`DW_TAG_inlined_subroutine` entries to write. The new `callers` part of the harness checks
what follows from that: a chain of an `#[inline(always)]` function inside an `#[inline]`
one, which rustc inlines even in a dev build and reports as inlined frames, prints the same
three frames from blaze, where they are real frames.

`#[track_caller]`. The same part panics through two nested `#[track_caller]` functions,
through one that reads `Location::caller()` into its message, through a `#[track_caller]`
trait method called on a `dyn` object, through `Option::unwrap` in a closure, and by
indexing out of bounds. The panic location (file, line, column), the message and the frames
are compared with the cargo build.

That part found one difference, which is fixed: an index out of bounds on an array or slice
was reported at the wrong column (`items[at]` at the bracket, and `let item = items[at];` at
the pattern), where rustc reports the start of the index expression. The bracket is right
only for a call of `Index::index` (a `Vec`), which is what the rule was written for. The
bounds check now takes its location from the index expression.

## Profile settings (round two)

The settings come from the build-model's plan (`crates/blaze-engine/src/plan.rs`, which takes them
from cargo's unit graph); `Cargo.toml` is not parsed again and `profile.rs` is not touched.
`Profile::described` turns `debuginfo`, `split_debuginfo` and `strip` into one of three
answers, nothing, lines, or full, with where it is kept, or into a refusal:

| setting | what blaze does |
|---|---|
| `debug = false`, `0`, `"none"` | no debug sections |
| `debug = 1`, `"limited"`, `"line-tables-only"`, `"line-directives-only"` | line tables and subprograms, no variables or types |
| `debug = true`, `2`, `"full"` | everything |
| any other `debug` value | refused: "blaze does not know the level" |
| `strip = "debuginfo"` or `"symbols"` (`true`) | no debug sections, whatever `debug` says |
| `split-debuginfo = "off"`, or not set | as above |
| `split-debuginfo = "unpacked"` with debug info on | a `.dwo` beside each object (round three) |
| `split-debuginfo = "packed"` with debug info on | the same, and a `.dwp` beside the executable (round three) |
| any other `split-debuginfo` or `strip` value | refused: "blaze does not know the setting" |
| `split-debuginfo` set with debug info off or stripped | accepted, there is nothing to split |

Round two refused `packed` and `unpacked`; they are done now, see "Split DWARF" under "Round
three". What is still refused is a value of `debug`, `strip` or `split-debuginfo` that blaze
does not know, and the build fails with that reason when it is planned.

`strip = "symbols"` also asks for the symbol table to go. That is the linker's work and is
not done by this code.

When several packages are built with different profiles, the most any of the packages cargo
was asked to build wants is used for all of blaze's code. The harness changes the setting
nine times in one workspace and checks the object after each. `BLAZE_DEBUGINFO=0|lines|1`
overrides it for measuring. Since the build model's second round `profile.rs` is gone: the
setting is a `plan::Described` read from the unit graph and kept through a reload, and a
project cargo does not plan gets no debug info.

## What is independent of the object format, and where a new one plugs in

There are three layers.

1. What a function's code says about itself. This is produced by the backends and stored in
   the cache with the code: line rows as (code offset, span index), and `SlotPlaces`, the
   frame base and the location of each named stack slot. The location expressions are DWARF
   expression bytes, because that is what LLVM hands back; for Cranelift they are a frame
   offset. Nothing here knows the object format, and nothing here has a line number in it.

2. The description of a function, `FunctionDebug` in
   `crates/hir-ty/src/mir/codegen/debuginfo.rs`: its name and linkage name, where it is
   declared, the rows with file, line and column looked up, a table of types (`DebugType`:
   base, pointer, array, aggregate with fields, tags and variants), variables with a type
   index and a place, and scopes. `debuginfo/types.rs` builds the type table from blaze's
   types and layouts. This layer is the format-independent part: it is the same for ELF,
   Mach-O and COFF, and it is what a CodeView writer would consume. The one DWARF-shaped
   thing in it is the location expression bytes of a variable that is not at a plain frame
   offset; a CodeView writer handles `VariablePlace::Frame` directly and has to translate, or
   leave out, the `Ranges` case (LLVM-optimized locals).

3. The writer, `DebugContext` in the same file. `add_function` turns a `FunctionDebug` into
   DWARF entries with gimli, and `emit` writes the sections and their relocations into the
   object. This is the only part that is specific, and it is specific in two different ways:
   to DWARF, and to ELF.

A new object format plugs in at `DebugContext::new` and `DebugContext::emit`. `new` returns
`None` for anything but ELF today, which turns debug info off for the object; `emit` adds
the sections by their ELF names with absolute relocations against function symbols and
section symbols.

- Mach-O keeps DWARF, so layers 1 to 3 stay. `emit` has to name the sections
  `__DWARF,__debug_*`, and use the relocation forms Mach-O has (section-relative
  references inside `__DWARF` are written as plain offsets, not relocations). Then the
  linker's debug map and `dsymutil` do the rest; see the Mach-O notes.
- Windows has a second writer, `CodeView` in `debuginfo/codeview.rs`, that takes the same
  `FunctionDebug` values and writes `.debug$S`; `DebugContext` is the enum of the two
  writers, picked from the target's binary format in `DebugContext::new`. A Mach-O writer
  is a third arm there.
- The splice path appends debug sections to a linked ELF (`crates/blaze-engine/src/splice`); that
  code is per format already and is separate from the above.

The harness is split the same way. In `harness/debuginfo.py` the comparison of backtraces
(`panics`, `own`, `compare`), the fixtures, the gdb and rust-gdb drivers, the profile checks
and the cost measurement do not look at the file format. What reads ELF or DWARF is in
functions of its own: `verify` and `described` (llvm-dwarfdump), `frames_of` and `unwinding`
(`.eh_frame_hdr`), `sections` (ELF section headers), `unit_of`, and `jumps_away` (dwarfdump
and objdump). Round two only added functions and parts (`callers`, `locals` under LLVM,
`types`, `profile`); the existing ones kept their names and arguments, except that `compare`
takes the binary as an optional last argument and `panics` keeps the message next to the
location.

## The splice path

Decision: append, do not patch.

The sections a debugger reads are not loaded, and each one is a single run of the file found
through the section headers. There is no room to grow one in place, and reserving room at
link time would mean guessing a size and still running out. A splice already starts from the
linker's file every time (it undoes the previous splice and truncates) and the patch object
is cumulative, so nothing has to be patched incrementally either.

So after the splice has written its loaded segments, for every `.debug_*` section the patch
object has, it writes a new copy at the end of the file: the executable's bytes first, then
the patch's, with the patch's relocations applied. A reference into another debug section
becomes the old size of that section plus the addend; a reference to code becomes the address
the splice placed that code at. A new section header table goes after that, with those
sections pointing at their new copies, and `e_shoff` in the ELF header is updated. The patch
is a compilation unit of its own, so the executable's units are untouched. The old code of a
replaced function is still in the file behind its jump and what the executable says about it
stays true.

If the executable has no section of a kind the patch has, the splice adds one: a new header
at the end of the table, its name added to a copy of the section names written the same way,
and `e_shnum` updated. Until this round that was refused. It happens with split debug info
under LLVM, where the executable's location lists are in the `.dwo` files and the patch,
which is not split, has a `.debug_loc`; three of the six edits of the `kept apart` run were
linked in full for it before. If a section is compressed (`SHF_COMPRESSED`), the splice is
refused with that reason and blaze links in full instead.

A function is put in the patch again not only when its code changes but when any of its
lines change, so adding a line at the top of a file re-describes the functions below it. Its
compiled code comes from the cache; only the line lookup is redone.

The code is in `crates/blaze-splice/src/debug.rs`.

## What was measured

All on one machine, Linux x86-64, Rust 1.99.0, LLVM 20, at the head of this branch.

    cargo build -p blaze-engine --release
    LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p blaze-engine --release --features llvm --target-dir target-llvm
    cd harness
    BLAZE_BIN=$PWD/../target/release/blaze-engine python3 debuginfo.py
    BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast BLAZE_BIN=$PWD/../target-llvm/release/blaze-engine python3 debuginfo.py

`debuginfo.py` exits 0 under both backends ("all right").
`--only corpus|stress|rung2|edits|callers|locals|types|scopes|rich|profile|cost` runs one part. The
programs used are the corpus runner, the `stress` workspace (three crates), `rung2`, and
small programs the script writes itself.

What it checks, and what it printed:

Backtraces. Each program is built by cargo and by blaze, each deliberate panic is run with
`RUST_BACKTRACE=1`, and the frames that belong to the workspace's crates are compared by
name, file, line and column. The panic location itself is compared as well, which is the
`#[track_caller]` check: the corpus and the edits program panic through `#[track_caller]`
functions.

| program | Cranelift | LLVM -O2 |
|---|---|---|
| corpus runner | 84 panics, 444 frames, all the same | 84 panics, 443 frames; 1 panic one frame short, the function seen to leave by a jump |
| stress | 1 panic, 3 frames, same | 1 panic, 2 frames; one short, seen to leave by a jump |
| rung2 | 1 panic, 3 frames, same | same |
| callers (round two) | 6 panics, 19 frames, same | 6 panics, 18 frames; one short (`sline::inner`, which ends in `expect`), seen to leave by a jump |

Under Cranelift the comparison is exact. Under LLVM at -O2 a panic passes if blaze's frames
are the cargo build's with some left out and every function left out is one that leaves by a
jump; see "Tail calls" above.

One name is normalized by the script: rustc calls the code that drops a coroutine
`core::ptr::drop_glue::<X>` and blaze has the coroutine's own poll function do it, so the
frame is named `X`. The file, line and column agree. This affects 14 of the 84 corpus panics.

`llvm-dwarfdump --verify`: "No errors." for every binary, including after each splice.

Unwind tables against debug ranges. For every range in a blaze unit the script looks for the
frame the unwinder would find (through `PT_GNU_EH_FRAME` and the search table, not the
section header) and checks that it starts at the same address and does not end after the
range. Corpus: 11647 functions, 0 without a frame, 0 where the frame ends past the range.
Three functions have neither a frame nor lines; they are `#[naked]` functions, which are
assembly only. Stress: 1834 functions, rung2: 3039, edits: 71 to 79 after each splice, all
agree.

gdb, batch mode: break at the file:line of the call into the panicking function, check gdb
stops there, that the pc is in a blaze unit, that `info line` gives that line, then `step`
four times and check it reaches other lines. Passes for corpus, stress, rung2 and for the
binary after the last splice under both backends. `print` of locals is the `locals` part
below.

perf: `perf record -F 5000 --call-graph dwarf` on the corpus runner, then
`perf script -F ip,sym,srcline`. Cranelift: 79 frames of blaze functions in 36 samples, none
without a line, 24 samples unwound back to `main`. LLVM: 46 frames in 22 samples, none without a line, 15 back to `main`. The machine needed
`sudo sysctl kernel.perf_event_paranoid=1`; where perf is not allowed the script says "not
run" and does not fail. No flamegraph tool is installed here, so no SVG was produced; the
`perf script` output is what `stackcollapse-perf` and `inferno` read.

Splice (`edits`): a program is built once under the daemon and edited six times: nothing, a
line added above everything, the panicking function made longer, its caller changed, a new
function added in the call chain, the added line removed. All 6 steps were spliced, and after
each the backtrace matches a fresh cargo build of the same source (7 or 8 frames),
dwarfdump verifies, and the unwind check agrees. Same under both backends.

Variables (`locals`): gdb stops in a function with three parameters and six locals and
prints each (`u32`, `f32`, `&str`, a struct, a tuple struct, an array, a tuple, and a
dereferenced reference). A value passes if it is right or if gdb says `<optimized out>`; a
wrong value or an unknown name fails. Cranelift: 9 of 9 right. LLVM -O2: 6 of 9 right and 3
`<optimized out>` in round two, 8 of 9 now. In the larger programs blaze describes 15,946 parameters and locals across
11,647 functions in the corpus, 3,006 in 1,834 for stress, 3,139 in 3,039 for rung2, the same
count under both backends.

Types (`types`, round two): the same program is built by cargo and by blaze, `rust-gdb` stops
at the same line in both and prints 21 values: a `u32`, a `&str`, a `[u64; 5]` passed by
value, a struct, a tuple struct, a tuple, three variants of an enum with fields (struct-like,
tuple-like, unit), a fieldless enum with explicit discriminants, `Some(42)`, `None`,
`Option<&Point>` both ways (a niche), `Result<u8, String>`, a `Vec<i32>`, a `String`, a slice
of the `Vec`, a union, a dereferenced reference and a sum. What blaze's binary prints has to
be what the cargo build's prints, character for character apart from addresses. Cranelift:
21 of 21. LLVM -O2: 20 of 21 in round two with one `<optimized out>` (the second field of the
tuple struct, which the optimizer splits apart), 21 of 21 now.

Profile (`profile`, round two): the nine settings in the table under "Profile settings", in
one workspace one after the other. All followed or refused as the table said then, under both
backends; there are eleven now, with the split settings followed.

lldb and addr2line were tried by hand on the locals program and are not in the script:
`addr2line -f -C -e bin 0x47387` gave `sline::area` and `src/main.rs:5`; `lldb-20 -b` with a
breakpoint on a line stopped, `frame variable` printed parameters and locals, `next` stepped
to the next line.

Cost (`cost`), corpus runner, best of three, compile and link of the `cases` crate object
(11,647 functions), with `BLAZE_DEBUGINFO=0` and without it:

| | Cranelift | LLVM -O2 |
|---|---|---|
| compile + link, no debug info | 5.48 s | 6.60 s |
| compile + link, debug info | 6.22 s (+13.4%) | 7.29 s (+10.4%) |
| object, no debug info | 41,502,552 bytes | 37,201,080 bytes |
| object, debug info | 48,150,544 bytes (+16.0%) | 44,799,664 bytes (+20.4%) |
| of that, sections a debugger reads | 3,249,209 | 3,442,559 |
| of that, their relocations | 3,398,783 | 4,156,025 |
| linked binary, no debug info | 86,944,048 bytes | 83,560,976 bytes |
| linked binary, debug info | 90,193,256 bytes | 87,003,536 bytes |

Round one had Cranelift at +21.9% with line tables and a few variables. With types and all
the variables of round two added it measured 5.41 s against 6.87 s (+27%) before the work
below, and +13.4% after. The object grew more than in round one (+10.7% then) because types
and variables are in it now. The LLVM figure moved between +7.8% and +12.7% over the runs of
this round; the +12.7% was taken while other builds ran on the machine, so read it as
"about a tenth". Timings vary by a few points from run to run.

Where the time went, from `perf record -g` on the daemon during the Cranelift build with
debug info (samples, so shares and not exact times): of about 420 ms of CPU in describing
functions, 180 was in working out types (`Types::describe`, `Types::ty`, `Types::fields`,
`Types::parameters`), 84 in turning a span into file, line and column, 68 in
`DebugContext::add_function`, 36 in copying type tables, and another 70 at the end in gimli
writing the sections and the object writer taking their relocations.

What was changed for it:

- The line starts of a file are a tracked query (`line_starts`), computed once for a file.
  Before, each span counted newlines from the start of the file.
- The types of one environment are described once for the whole object (`ObjectCodegen`
  keeps a `Types` for each `StoredParamEnvAndCrate`), with the name and the normalized form
  of a type remembered. Before, every function described its types from nothing.
- A function is given a copy of only the part of the type table it uses, with the indices
  moved (`add_types`).
- Units of 256 functions (this was done for `addr2line` and perf, which took over a second
  for the first lookup in one unit of 11,647 functions; it does not make the build faster).

What is left of the cost is mostly real work: the type graphs, the line rows of each
function, and about 3.4 MB of relocations for 3.2 MB of sections, which the object writer and
then the linker both go through. The relocations could be cut by describing addresses
relative to one symbol for a unit (DWARF 5 `DW_FORM_addrx` and range lists against a base);
not done.

Regression gate, both backends, on the tree that was committed (the report was edited after):

| | Cranelift | LLVM -O2 |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 0 of 893 layouts differ | MATCH, 0 of 893 |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 43 of 43, 5 open | 43 of 43, 5 open |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 58 of 58 | 58 of 58 |
| `keys.py` | 36 of 36, 0 open | 36 of 36, 0 open |
| `debuginfo.py` | all right | all right |
| `storekills.py --kills 100` | PASS | PASS |
| `storecorrupts.py --rounds 4` | PASS | PASS |
| `storefull.py` | PASS | PASS |
| `storeshared.py` | PASS, 180 builds | PASS, 180 builds |

`buildmodel.py` runs both backends itself, with the binary built with `--features llvm`:
180 of 180 rows as expected, 0 WRONG. That is 176 in the full run plus the four
`cross_musl` and `cross_aarch64` rows run again on their own, because the 1.99.0 toolchain
on this machine did not have those two targets installed during the full run and cargo
itself could not build the cases.

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine`: 930 passed, 0 failed, 1 ignored. `cargo clippy -p hir-ty -p blaze-engine` has
no warning in the files this branch added.

## The gdb rows on the GitHub runner

In CI (run 39034233840) every `gdb at <file>:<line>` row was `NOT STOPPED`, with gdb's
`[Inferior 1 (process N) killed]` shown as the reason, under both backends. The debug info
was right and the harness was wrong.

The runner is `ubuntu-24.04` with gdb 15.1; this machine is Ubuntu 22.04 with gdb 12.1. I
ran the corpus row in an `ubuntu:24.04` container with its gdb. gdb set the breakpoint,
stopped at it and stepped, and printed the stop as

    Breakpoint 1.1, ... at cases/src/lib.rs:1962

where gdb 12 prints `Breakpoint 1, ...`. gdb from version 13 on numbers the location when a
breakpoint has more than one, and a line of a generic function or one with code in two
places has more than one. The script looked for `Breakpoint 1, `, did not find it, and
reported the last line gdb printed, which is the inferior being killed when the batch ends.
It now accepts `Breakpoint 1.N, ` as well. The locals and types rows already passed in CI
because they read values and not the stop line.

The other suspects, as seen in the container: `personality` is refused there
(`warning: Error disabling address space randomization: Operation not permitted`) and gdb
goes on with ASLR left on, which does not matter for a breakpoint by file and line; there
was no debuginfod prompt in batch mode; the breakpoint was not pending; ptrace worked. gdb
also warns that the `gdb_load_rust_pretty_printers.py` script named by `.debug_gdb_scripts`
is not on its path, as it does for any Rust binary run under plain `gdb`.

With that change pushed (commit 22e1ec56d6, run 39044537945) both harness jobs pass on the
runner and every gdb row is `stopped`: corpus `lib.rs:1962`, stress, rung2 `main.rs:134` and
the binary after the last splice, with locals 9 of 9 and types 21 of 21 under Cranelift and
6 of 9 with 3 optimized out and 20 of 21 with 1 optimized out under LLVM, the same as here.
No row is skipped on CI. The `Coverage` workflow fails on this branch as it does on
`blaze-collate`; I did not look into it.

## What is open

Inline frames. There are no `DW_TAG_inlined_subroutine` entries because nothing is inlined;
see "Tail calls, inlining and `#[track_caller]`". If blaze starts inlining (LLVM across
functions, or MIR inlining) the entries have to be added at the same time, or backtraces
will lose frames.

Under LLVM -O2 three values that `rustc -O -g` shows are `<optimized out>`: `width` in the
`locals` part, `step` and the `step` half of `value` in `scopes`. The cause and what was
tried are under "LLVM and optimized-out values". Closing it needs blaze's LLVM to see the
callee, that is inlining, which also needs `DW_TAG_inlined_subroutine` (above).

Locals that Cranelift keeps in a register and unsized locals are not described. I did not
count how many variables rustc describes for the same code.

Split debug info. A splice into an `unpacked` program is checked (`edits, kept apart`); one
into a `packed` program is not, and neither is a real project with either setting. The
split writer is little-endian only.

The real-project check (`realdebug.py`) is not in the gate or in CI: it clones from GitHub
and takes minutes.

After a splice, the `.eh_frame` section header still covers only the linker's frames. The
frames of spliced code are in the appended segment and are in the `.eh_frame_hdr` search
table, which is what the unwinder and perf use, and backtraces through spliced code are
right. A tool that reads `.eh_frame` by section name (`llvm-dwarfdump --eh-frame`) does not
see them. Spliced functions also have no `.symtab` entry; gdb names them from DWARF.

A breakpoint set by function name can land on the old copy of a replaced function, which is
still described by the executable's unit. Breaking by file:line finds the new code.

An edit that changes only a type (say a field's name) without changing the code of a function
that has a local of that type does not re-describe that function in the patch, so the
debugger shows the old field name until the next full link.

The coroutine drop frame name (`X` where rustc says `core::ptr::drop_glue::<X>`), described
above. Fixing it means emitting a real drop-glue function for coroutines instead of polling
with a null context.

Compressed debug sections in the executable make the splice fall back to a full link.

gdb prints a warning about `.debug_gdb_scripts` for these binaries; it comes from rustc's
std objects and is the same for the cargo build.

## Where the work stopped, and what I would do next

Done: everything in "At this head", on ELF under both backends, with `blaze-collate-next`
merged and the gate run once on that.

Half done: split DWARF (`unpacked` is checked through a splice, `packed` only on the
fixture and never through `blaze`); the cost, which is measured and not yet profiled
for this round; the Windows side of `debuginfo.py`, merged and not run.

Next, in this order: run `debuginfo.py` on the Windows and AArch64 runners and read the
rows; write the debug sections with fewer relocations (one symbol a unit, DWARF 5
`addrx`), since the relocations are as large as the sections; a `packed` program through a
splice and through `blaze`; the three LLVM values, which need inlining and the
inlined-subroutine entries with it; variables and types in CodeView; Mach-O.

## Mach-O (design only)

On macOS the linker does not copy DWARF into the executable. It writes a debug map (`N_OSO`
stabs naming each object file, `N_FUN` for functions) and `dsymutil` later gathers the DWARF
from the objects into a `.dSYM` bundle; lldb can also read the objects directly through the
debug map.

For a full link this is mostly free: the gimli writer already produces the sections, and
they need to go into `__DWARF,__debug_*` with Mach-O relocations (`object` supports this; the
relocation kinds for section-relative references differ, `X86_64_RELOC_UNSIGNED` /
`ARM64_RELOC_UNSIGNED` and subtractor pairs). The backtrace in a Rust binary on macOS reads
the dSYM or the objects through the debug map, so blaze's object has to stay on disk where
`N_OSO` says or `dsymutil` has to be run after the link.

For a splice the debug map is the natural place: add an `N_OSO` entry for the patch object
plus `N_FUN` entries at the spliced addresses, and leave the DWARF in the patch object.
The Mach-O splice would have to rewrite the symbol table in `__LINKEDIT` for this; I have
not checked how much of that it already does. A stale `.dSYM` next
to the binary would win over the debug map, so the splice has to remove or regenerate it.

Estimate: one week for full-link objects with backtrace and lldb checks on aarch64 macOS, one
more for the splice and for a macOS variant of `debuginfo.py` (lldb in place of gdb,
`dwarfdump --verify`, `atos`). I have not run any of this.

## Windows (the design; WINDOWS.md has what was built)

Lines and procedures are written since, under both backends, and checked by `debuginfo.py`:
the last section of WINDOWS.md has what was done and measured. Types and the PDB after a
splice are still as described here.

MSVC targets use CodeView: `.debug$S` (symbols and line tables per function) and `.debug$T`
(types) in the COFF object, which `link.exe` or `lld-link` merge into a PDB.

LLVM can write CodeView itself (module flag `CodeView`, the same DIBuilder metadata), so the
LLVM backend is the short path, but today blaze reads LLVM's line table back and writes the
sections itself, and that part would need a CodeView reader or a mode where LLVM's sections
are kept as they are. Cranelift has no CodeView writer, and as far as I know rustc's Cranelift
backend does not have one either. Writing `.debug$S` line tables and `S_GPROC32` records by hand is
well specified (the `C13` line format) and small for lines and names; types (`.debug$T`,
TPI/IPI hashing) are a lot more work.

The splice is the hard part. A PDB is a multi-stream file that is not designed to be
appended to; the realistic options are to rewrite the PDB after each splice (the `pdb` crate
only reads; LLVM's PDB writer is C++ and not exposed through the C API), or to emit
`*-windows-gnu`-style DWARF in the PE for spliced code only, which WinDbg and Visual Studio
do not read. Windows backtraces in Rust go through `dbghelp` and need the PDB.

Estimate: two weeks for line tables and procedure symbols in full links under both backends
with a `cdb`-driven harness, two to four more for the splice depending on which PDB route is
taken, and types on top of that. This is the least certain number here.
