---
status: record
summary: wasm32 as an emit target.
date: 2026-10-20
---
# wasm32 as an emit target for blaze

This is the report for the `blaze-wasm32-target` branch: the daemon runs natively and
compiles a workspace's crates to wasm32, links them ahead of what cargo built for the same
target, and answers an edit without the linker.

The numbers here are of that branch. Those at the current commit, measured 2026-10-27 on the
pinned nightly and checked by the train on every push to main, are in
[platforms.md](../platforms.md), wasm32.

Everything below is either measured (I ran it, on this Linux x86-64 VM, 8 cores, with the
toolchain in `harness/toolchain`, `nightly-2026-05-10`) or marked as read from code or
inferred. Timings are medians over the edits of one run unless they say otherwise; the
machine was otherwise idle for the numbers in the tables, apart from where noted.

## Short answers

1. Codegen. `blaze-wasm` emitted wasm function bodies from Cranelift IR, not from MIR, and
   linked them itself. That lowering is now the wasm code producer of the main workspace,
   behind the target, and a new object writer wraps it as a relocatable wasm object with
   linking metadata that `rust-lld` links ahead of rustc's rlibs. Memory, indirect calls
   through the function table, 64-bit and 128-bit values and the C ABI work as far as the
   harnesses exercise them. Unwinding is not emitted: the wasm targets are `panic=abort`.
2. Differential test. The corpus runs under wasmtime for `wasm32-wasip1` and matches
   cargo's build, with 655 of 656 functions compiled by blaze (the same one function falls
   back as on x86-64).
3. The edit loop. Three options were compared; the one built rewrites the linked module
   with the patch's functions in place of its own, which takes about a millisecond for a
   small program and 20 to 50 ms for a Dioxus app (writing 29 MB), against 40 ms and
   180 ms for `rust-lld`. It splices more edits than the x86-64 splice does, because wasm
   has nothing to jump over and no thread-locals.
4. wasm-bindgen costs 0.27 to 0.36 s per edit on the hello world, which is more than the
   compile, the splice and the link together. A patch cannot skip it today; what it would
   take is below.
5. Measured: a small std program goes from a saved edit to a program that can be run in
   11 ms with the splice and 57 ms with the full link (x86-64 on the same machine and edit:
   12 ms and 41 ms). The Dioxus web hello world takes 35 ms with the splice, 193 ms with the
   full link, plus 0.3 s of wasm-bindgen; the page then shows the edit in headless Chrome.

Every harness runs for `wasm32-wasip1` and passes, apart from `asmedits.py` and `deps.py`,
which are not about the target (see "Harness results"). The x86-64 numbers are unchanged.

## What was built

- `crates/hir-ty/src/mir/codegen/target.rs`: the target the process compiles for,
  `X86_64Linux` or `Wasm32`, chosen once from the triple the daemon is given with
  `--target`. Everything that differs asks it: the width of an address (`word()`), what
  writes a function's code, what an object file is, and the C ABI.
- `crates/hir-ty/src/mir/codegen/wasm.rs`: the lowering of Cranelift IR to a wasm function
  body, from `blaze-wasm`, changed to emit code for wasm32 that links with rustc's: direct
  `call`s by symbol (relocations instead of its own table), `__stack_pointer` as the
  linker's global, and LLVM's signatures (a function that returns more than one value or an
  `i128` takes a pointer ahead of its parameters). Its `Calls::Table` mode, where every call
  goes through the table, is kept but unused.
- `crates/hir-ty/src/mir/codegen/wasm_object.rs` (new): turns the object `cranelift-object`
  builds, whose "machine code" is those function bodies, into a relocatable wasm object:
  type, import, function, code and data sections, a `linking` section with the symbol table
  and segment info, and `reloc.CODE`/`reloc.DATA` with `R_WASM_FUNCTION_INDEX_LEB`,
  `TABLE_INDEX_SLEB/I32`, `MEMORY_ADDR_SLEB/I32`, `TYPE_INDEX_LEB` and `GLOBAL_INDEX_LEB`.
  Each static is a segment of its own so that the linker drops the ones nothing refers to.
- `codegen.rs`: target-sized pointers and layouts throughout (`word()` instead of 8),
  the wasm C ABI, C variadics on wasm (the trailing arguments are spilled to a buffer whose
  address is passed last, as clang does), and `TypeId` hashes that take the target's word
  into account. `symbols.rs` in the daemon reads data symbols out of wasm rlibs, which the
  `object` crate reports without a section.
- `crates/blaze-splice/src/wasm.rs` (new): the splice of a wasm module, described under
  "The edit loop".
- Harnesses: `BLAZE_TARGET=wasm32-wasip1` makes every harness build cargo's side for that
  target, capture `rust-lld`'s line instead of `cc`'s, run programs under wasmtime, use
  `wasm-tools` where they used `strip` and `readelf`, and skip valgrind. `editloop.py` is
  new and measures the times in this report.

Nothing changes for x86-64 apart from refactoring: the full set of harnesses was rerun on
`x86_64-unknown-linux-gnu` after the last change (results below).

## 1. Codegen

### What `blaze-wasm` emitted from

Read from code: the `blaze-wasm` branch did not emit from MIR. It ran the same pipeline as
native blaze up to Cranelift IR (`FunctionCx` in `codegen.rs` builds IR), and then, instead
of giving Cranelift the IR to compile, lowered the IR to a wasm function body itself
(`wasm.rs`, about 2,200 lines: a stack machine over Cranelift's SSA values, locals for
values that are live across blocks, structured control flow recovered from the CFG). Its
own linker (`crates/blaze-wasm/src/link.rs`) put the bodies into a module with a table
where every call was `call_indirect` through the callee's slot, which is how it swapped
functions. It compiled `core` from source for `wasm64` and had no `std`.

So the question "what does it take to make that emitter produce wasm32 objects" became: keep
the lowering, drop its linker, and write what the linker expects from an object.

### What it took

- Address width. The IR assumed 64-bit pointers in many places (layouts, `TypeId`, vtables,
  slice metadata, constants). These now ask the target (`word()`); `blaze-wasm` had moved
  some of these already.
- Relocatable output. `cranelift-object` still builds the ELF object (it is what holds the
  symbols, relocations and data); `wasm_object::from_elf` reads it back and writes the wasm
  object. A function's "code" in that ELF is the wasm body plus a header that lists its
  signature, the types it uses in `call_indirect` and where its `__stack_pointer` accesses
  are, so that the writer can emit the type section and the right relocations. Cranelift's
  relocations against symbols become `R_WASM_*` relocations at the five-byte padded LEBs
  the lowering leaves.
- Signatures. The object links with code LLVM compiled, so a function's wasm signature has
  to be the one LLVM gives it. The lowering flattens scalar pairs as LLVM does and returns
  more than one value or an `i128` through a pointer; `codegen.rs` has a wasm C ABI for
  `extern "C"` (everything that is not a scalar goes by pointer).
- Imports. Every function and global the object names and does not define is an import
  from `env` with its signature, which `rust-lld` resolves against the rlibs. A signature
  that disagrees with the definition is a linker warning, and the call then reaches a stub
  that traps; the harnesses ran without one after the ABI was right.

### What is there and what is missing, for real programs

Measured, by the corpus and the other harnesses under wasmtime:

- Memory: statics, `.bss`, constants, the stack in linear memory through `__stack_pointer`,
  allocation through rustc's `alloc` and `dlmalloc`. Works.
- Indirect calls and the function table: function pointers are table slots
  (`TABLE_INDEX_*` relocations), `call_indirect` with a type relocation, vtables as data
  with table-index entries, closures, `dyn Trait`, `Fn` pointers. Works.
- 64-bit and 128-bit values: `i64` is native; `i128` goes through the lowering's wide
  integer support and LLVM's convention of passing it by pointer. The corpus has `u128`
  arithmetic, formatting and overflow checks. Works, apart from the wide-integer opcodes the
  lowering reports as unsupported, which no harness function hit.
- The C ABI: `extern "C"` calls into libc (`wasi-libc`) including variadics (`printf`),
  callbacks from C into Rust through function pointers. Works. wasm-bindgen's imports and
  exports are `extern "C"` with scalars and pointers, which is this ABI; the Dioxus hello
  world calls through them (measured, below).
- Unwinding: not emitted. `try_call`'s exceptional edges are dropped, which is what
  `panic=abort` means, and `wasm32-unknown-unknown` and `wasm32-wasip1` are `panic=abort` in
  rustc's sysroot. `catch_unwind` runs the closure and never catches; a panic prints its
  message and traps. The corpus's unwinding cases are gated with `cfg(panic = "unwind")` in
  the fixture, so they are not compared on wasm. `unwinds.py` passes because both sides abort
  at the same point with the same output.
- Threads: `wasm32-wasip1` has none, so the fixtures that spawn threads run them inline on
  wasm (`splices.py`) or are gated out (two corpus cases). Thread-locals are plain statics on
  the target, which is why the wasm splice never declines for TLS where the x86-64 one does.
- SIMD: the x86 intrinsics in the corpus and `breakers` are gated to x86-64; the lowering has
  no `v128`.
- Not tried: `wasm32-wasip2`, `+atomics`/shared memory, exception handling, reference types
  beyond `funcref`, multi-memory.

Inferred: the function table being the linker's means the daemon's object can take the
address of any function in any rlib (the linker allocates the slot), and rustc's code can
take the address of a blaze function, which is what `vtables` and `fn_pointers` in the
corpus show for the second.

## 2. Differential test

`BLAZE_TARGET=wasm32-wasip1 python3 harness/difftest.py corpus --bin runner --crate cases`:

    cases: 655/656 functions from blaze
    MATCH

Measured. The runner's output is identical between the module blaze linked and
the one cargo linked, both under `wasmtime run -W max-wasm-stack=8388608 -S inherit-env`.
The one function not from blaze is the `TypeId` of a `fn(&u8) -> &u8` type, which is the one
that falls back on x86-64 too (679/680 there). The fixture changes for the target are: the
unwinding cases gated with `cfg(panic = "unwind")`, two thread cases gated with
`cfg(not(target_family = "wasm"))`, the x86 SIMD cases gated with
`cfg(target_arch = "x86_64")`, and `%lu` became `%llu` in the variadic case because `long`
is 32 bits on wasm32 and the argument is a `u64`.

Three bugs the corpus found and that are fixed on the branch: `TypeId` hashes included the
width of `usize` as 8, the alignment of a static in its own segment was derived from its
address rather than capped by its size, and data symbols in wasm rlibs were not seen as
defined because `object` gives them no section, so the daemon compiled fewer functions.

`wasm32-unknown-unknown` is not run through the corpus: there is no runtime for it that
prints, which is what the differential test compares. It is what the Dioxus app is built
for, and that app runs (section 5). What differs between the two targets in blaze is nothing
(the same `Target::Wasm32`); what differs in cargo's side is `wasi-libc` and `_start`.

## 3. The edit loop

### What "splice" means for wasm

On x86-64 the splice appends the patch's code to the executable and writes a jump over each
replaced function's entry, because callers hold addresses. In wasm nothing holds an address
of code: a direct call names a function index, a function pointer is a table slot, an export
is a name, and each of those stays valid when the body under the index changes. So the
natural splice is to rewrite the module with the patch's body where the old body was, and
there is no jump, no stub and no unwind table to update.

The three options, compared:

1. Relink with `rust-lld` each time. Measured: 40 ms for the small program (1.1 MB module),
   180 ms for the Dioxus app (29 MB with debug info, 277 ms when the symbol table is kept).
   That is already far below cargo's rebuild, and it is what every harness runs when the
   splice declines.
2. Patch the module in place. This is what was built (`crates/blaze-splice/src/wasm.rs`).
   The daemon reads the linked module once after each full link (its symbol table, types,
   bodies, data segments, the table's element segment, the memory limits), and for each
   patch writes a new file: the code section with the patch's bodies under the indices of the
   functions they replace and new functions appended, the function and type sections grown
   as needed, the element segment grown by a slot per new function whose address is taken,
   a data segment after the linker's for each static the patch gives a new value (the same
   address, when it is no larger), the patch's new data in pages added to the end of the
   initial memory (which nothing of the program has claimed, since the heap is the rest of
   the first pages and what `memory.grow` adds), and the `linking` and `reloc.*` sections
   dropped. The patch's relocations are resolved against the module's symbol table by
   name, with signature checks. Because wasm has no in-file offsets other than section
   lengths, "in place" means a full rewrite of the file; the module in memory is the
   baseline and each splice starts from it, which is also how the x86-64 splice undoes the
   previous one.
   What it needs from the link: `--emit-relocs`, so that the linked module keeps its symbol
   table (otherwise it names nothing but its exports). The harnesses add that, and
   `--no-gc-sections` so that a function an edit starts calling is still there; for the
   Dioxus app `--no-gc-sections` doubles the module to 63 MB and wasm-bindgen then fails on
   it (an index out of bounds in its descriptor reader), so the Dioxus measurements keep gc
   on, and an edit that calls into a dropped function links in full.
   When it declines (measured, from the harness logs): the whole object was written again
   (an edit that changes the set of functions, same as x86-64), a function's signature
   changed, a static grew, a function the executable was linked with a patch of reverted to
   its first form, the file was written by someone else, and a name the patch refers to is
   not in the module (gc'd, or a `signature_mismatch:` stub).
3. A side module loaded at runtime that replaces table slots (subsecond's way). Not built.
   It is the only option of the three that keeps the program's state, which is what a hot
   patch in a running page needs; options 1 and 2 give a module that the page reloads. The
   pieces it needs are on the branch or known: the lowering's `Calls::Table` mode makes
   every call go through the table so that a slot write redirects callers; the object writer
   gives a patch whose code relocations are the same as for the full link; what is missing is
   the loader, which is what subsecond's `packages/subsecond` does for wasm: grow the table
   and the memory, instantiate the patch with `__table_base`/`__memory_base`, apply its data
   relocations against the running instance, run its constructors, then write the slots. The
   daemon would emit the patch as a module (not an object) with its imports named for the
   running instance's exports, which means the main module has to export every function and
   static a patch may refer to (what `--export-all`/`--emit-relocs` and subsecond's
   `wasm-split` post-processing do). I estimate this at a few days of work on top of what is
   here, and it changes the shape of the output (two modules and a runtime) rather than the
   compiler, which is why I did not start it without asking.

Which fits: option 2 for the loop as blaze runs it today (the file on disk is the program,
and the harnesses run it fresh), option 3 for Dioxus's development server, where the gain is
keeping the page's state and skipping wasm-bindgen. The two share the codegen and differ in
the last step.

### Measured, edit to a program that can be run

`harness/editloop.py` starts the daemon on a program, then replaces a line and puts it back
N times, asking the daemon after each (`blaze sync`), which compiles the change, writes the
patch, and links or splices; with `--run` it runs the result and checks the output changes
each time; with `--after CMD` it times a command on the linked program (wasm-bindgen).

Small std program: the `stress` fixture's `sapp` (three crates, `std`, formatting, `Vec`,
closures, 1,760 functions compiled by blaze), the edit changes the body of a generic
function with eight instances (`report::line`). 5 rounds (10 edits), the same edit on both
targets, one after the other on an idle machine.

| | sync (compile + patch + link or splice) | of which the link | run |
|---|---|---|---|
| wasm32-wasip1, full link | 57 ms (44 to 99) | `rust-lld` 39 to 83 ms | wasmtime 28 ms |
| wasm32-wasip1, splice | 11 ms (8 to 33) | splice 1.2 to 1.3 ms | wasmtime 27 ms |
| x86-64, full link | 41 ms (34 to 71) | `cc` 24 to 59 ms | 1 ms |
| x86-64, splice | 12 ms (7 to 23) | splice 0.2 ms | 1 ms |

The wasm module is 1.07 MB linked plainly and 2.31 MB with `--emit-relocs --no-gc-sections`
(the x86-64 executable is 2.48 MB). Cold: cargo's build of the three crates was cached
(0.3 to 0.6 s); the daemon's start to its first link, which includes loading the workspace
and compiling every function, was 1.9 to 2.1 s for wasm and 2.8 to 3.6 s for x86-64. An
earlier run with a smaller edit (one function, one instance) gave 38 ms and 11 ms on wasm;
the compile of the edit is most of what the splice row has left.

Dioxus web hello world (`dioxus = "0.7.10"`, `web` feature, a counter whose label is built by
a function the edit changes; `wasm32-unknown-unknown`; dependencies at `opt-level = 1`,
see section 4 for why). 5 rounds.

| | sync | of which the link | wasm-bindgen |
|---|---|---|---|
| full link | 193 ms (184 to 210) | `rust-lld` 177 to 186 ms | 266 ms (253 to 301) |
| splice (`--emit-relocs`) | 35 ms (30 to 67) | splice 22 to 46 ms | 348 to 695 ms |

The splice's time is mostly writing the 29 MB module (that size is debug info; the
`.debug_*` sections are copied through, and could be dropped from the rewrite). The
wasm-bindgen column is noisier in the splice run because the harness and the daemon were
busier; standalone on the spliced module it measured 0.27, 0.34 and 0.36 s on three runs.
Cold: cargo's build of the app and its 236 crates took 25 s (36 s with the dependencies at
`opt-level = 1`); the daemon's start to its first link took 8.4 s (full) and 8.9 s
(`--emit-relocs`), of which resolving the 236 crates is 6.8 s and compiling the app's 4
functions with their 3,248 instances is 1.1 s.

### Measured, the splice harnesses

`BLAZE_SPLICE=1 BLAZE_TARGET=wasm32-wasip1 python3 harness/splices.py`: 71 of 71 right, 62 of
the 71 steps spliced (x86-64: 71 of 71, 61 spliced). The nine that linked in full: three
where the whole object was written again (the set of functions changed), three after the
harness touched the binary ("written by someone else", as intended), one where a static
grew to 1 MB, one where a promoted constant of the same size came back with a larger
alignment than its address has, and one where the edit calls `abs`, which the linker had
dropped (that step keeps `--no-gc-sections` off). The x86-64 run has the same shape, plus
one "Text file busy" where an edit arrives while the binary runs, which a wasm module does
not have since wasmtime does not map the file.

`BLAZE_SPLICE=1 BLAZE_TARGET=wasm32-wasip1 python3 harness/stress.py`: 99 of 99 right, 0
restarts, 72 of 89 links spliced (x86-64: 64 of 89; the difference is the 14 patches with
thread-local data that the x86-64 splice declines). The 17 full links on wasm: six first
links, four after the object was rewritten, six signature changes, one reverted function.

The other daemon harnesses with `BLAZE_SPLICE=1` on `wasm32-wasip1`: `worktrees.py` 52 of 52
(39 of 44 links spliced, the same as x86-64), `programs.py` 22 of 22 (13 of 17),
`includes.py` 9 of 9 (6 of 8), `unwinds.py` 8 of 8.

One thing the wasm splice has to know that the ELF one does not: when a function's
signature changes, the full link gets a definition from blaze's object and a stale one from
cargo's `.rcgu.o` with different signatures, and `rust-lld` keeps both, lists neither in the
symbol table, and routes each caller to the one of its own signature (the other gets a
`signature_mismatch:` stub). The splice reads the `name` section for those and takes the
definition whose signature matches; without that, every edit of `sapp` after a signature
change linked in full (25 of 89 spliced instead of 72).

## 4. wasm-bindgen

Measured: `wasm-bindgen --target web` on the hello world's module takes 0.27 to 0.36 s
standalone, 0.25 to 0.30 s in the full-link loop, and up to 0.7 s when the machine is busy.
On this app that is more than compile, patch and splice (35 ms) and more than the full link
(193 ms). Its output, `dxhello_bg.wasm`, is 3.1 MB from the 29 MB input: it drops the debug
info and the symbol table and renumbers the functions (11,161 become 7,571).

Two things found on the way:

- wasm-bindgen 0.2.129 fails on cargo's own debug build of this app with the nightly in
  `harness/toolchain` ("index out of bounds: the len is 0 but the index is 0" in
  `descriptor.rs`), with or without blaze. It works on the same source built with stable
  1.94, and on nightly when the dependencies are built with `opt-level = 1`
  (`[profile.dev.package."*"]`), which is the profile the measurements use. Not blaze's
  bug, but anyone measuring this loop with this toolchain hits it first.
- With `--no-gc-sections` the same failure appears on the 63 MB module, so the splice's
  "keep everything an edit may call" setting is not usable with wasm-bindgen on this app.

Can a patch skip it? Not with what is built, and inferred for what could be:

- The splice as built produces the pre-bindgen module, so wasm-bindgen has to run again.
  Its output is a function of the whole module (it walks every export and descriptor), and
  it has no incremental mode.
- Splicing into wasm-bindgen's output instead is plausible for body-only edits. Read from
  the output: it keeps the `name` section, the same table 0 (1,125 slots), the same memory
  (21 pages), the same data addresses and `__stack_pointer`. So a replaced body can be
  found by name and the relocations against data resolved with the addresses from the
  pre-bindgen symbol table, which the daemon has. New functions and new table slots would
  work the same way. What would not: an edit that adds or changes a wasm-bindgen import or
  export (a new `web_sys` call the module did not use), since those are what wasm-bindgen
  generates JS for, and the descriptors wasm-bindgen reads are gone from the output.
  Estimate: a day or two, on top of `splice/wasm.rs`, and the page still reloads.
- Keeping the page's state as well is option 3 of section 3 (the side module), which skips
  wasm-bindgen by construction: the running instance already went through it, and the
  patch imports from the instance.

## 5. Measured timings, summary

| program | cargo cold | daemon to first link | edit, full link | edit, splice | wasm-bindgen |
|---|---|---|---|---|---|
| `sapp` (wasm32-wasip1) | 0.4 s (cached) | 2.0 s | 57 ms | 11 ms | - |
| `sapp` (x86-64, for comparison) | 0.4 s (cached) | 2.8 s | 41 ms | 12 ms | - |
| Dioxus hello (wasm32-unknown-unknown) | 25 s | 8.4 s | 193 ms | 35 ms | 0.27 to 0.36 s |

"Edit" is from the daemon receiving the request after the file changed to the program on
disk being ready to run (or to hand to wasm-bindgen); it includes compiling the edited
function, writing the patch and the link or the splice. The file watcher's latency is not in
it (the harness asks the daemon explicitly, 20 ms after writing).

The Dioxus page: the spliced module, after wasm-bindgen, served with the generated JS and
loaded in headless Chrome, renders the label from the edited function and the button
increments the counter. Checked for four consecutive edits (the label alternates between the
two texts). This is measured for the module on disk; no hot patch into a running page was
done.

## Harness results

`wasm32-wasip1`, this branch, `BLAZE_TARGET=wasm32-wasip1`:

| harness | result | notes |
|---|---|---|
| `difftest.py corpus` | MATCH, 655/656 from blaze | fixture gates for unwind, threads, x86 SIMD; `%llu` |
| `stress.py` | 99 of 99, 0 restarts | with `BLAZE_SPLICE=1`: 99 of 99, 72 spliced |
| `breakers.py` | 15 of 15, 1 open, 0 skipped | `future_size` open as on x86; `clmul`, `simd_cast` gated to x86 |
| `unwinds.py` | 8 of 8 | both sides abort at the same point; nothing is caught |
| `BLAZE_SPLICE=1 splices.py` | 71 of 71, 62 spliced | threads run inline on wasm; `wasm-tools` for `strip`/`readelf` |
| `BLAZE_SPLICE=1 worktrees.py` | 52 of 52, 39 spliced | |
| `includes.py` | 9 of 9 | 6 spliced with `BLAZE_SPLICE=1` |
| `programs.py` | 22 of 22 | 13 spliced with `BLAZE_SPLICE=1` |
| `asmedits.py` | not run for wasm | the fixture is x86-64 `asm!` (its docstring says so); there is no wasm inline assembly in Rust to port it to |
| `deps.py` | not run for wasm | it tests the daemon's cache of cargo's dependency builds, which has no target-specific code; it ran on x86-64 |

`x86_64-unknown-linux-gnu`, this branch, every harness rerun after the last change:
corpus MATCH with 679/680 from blaze, programs 22 of 22, stress 99 of 99 with 0 restarts
(with `BLAZE_SPLICE=1`: 99 of 99, 64 of 89 links spliced), breakers 15 of 15 with 1 open,
unwinds 8 of 8, `BLAZE_SPLICE=1` splices 71 of 71 with 61 spliced, `BLAZE_SPLICE=1`
worktrees 52 of 52, asmedits 10 of 10, deps 10 of 10, includes 9 of 9. The same as
`blaze-spike`.

## What is left, ranked

1. The side module (option 3): a patch that a running page loads, so that the state stays
   and wasm-bindgen is not run again. Codegen in `Calls::Table` mode exists; the module
   writer for a patch and the loader do not. This is the piece Dioxus would use.
2. The daemon should drop `.debug_*` from the spliced module when the input has them, or
   splice into a module linked without them: 29 MB of writing per edit is most of the
   splice's 35 ms.
3. Keep the splice from declining on signature changes by having the full link not include
   cargo's stale `.rcgu.o` for crates blaze compiles (`BLAZE_ALONE` does this for the
   differential test), which also removes the `signature_mismatch:` stubs.
4. `wasm32-unknown-unknown` through the corpus: a tiny runtime (Node with an `env` that
   prints) would let the differential test run there too, rather than relying on the WASI
   run and the Dioxus page.
5. The lowering's gaps: the wide-integer opcodes it rejects, `v128`, and `try_call` for a
   future `panic=unwind` with exception handling. None blocked a harness.
6. Splicing into wasm-bindgen's output for body-only edits (section 4), if option 1 is not
   taken.
7. `BLAZE_LINK_ARGS` in the harnesses chooses between `--no-gc-sections` and wasm-bindgen
   working; the daemon could instead keep a list of what the linker dropped and declare the
   splice unable for those names up front, which it does today by failing the lookup.
8. Threads (`wasm32-wasip1-threads`, `+atomics`): thread-locals become real TLS again and
   the splice's data handling needs the x86-64 splice's refusal.
