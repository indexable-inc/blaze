# blaze in the browser

This is the report for the `blaze-wasm` spike: can blaze's compiler run in a web page,
compile Rust there, and swap edited functions into a running program?

Short answer: yes. The page in `crates/blaze-wasm/web` is a playground: a Dioxus app with
an editor on the left and a running program on the right. It loads blaze built as wasm32,
compiles the program in the editor to wasm32, and on each edit compiles only the functions
that changed and swaps them in while the program keeps its memory. Four of the programs are
`#![no_std]` and draw on a canvas, with the parts of `core` they use compiled from source in
the page. The fifth is a Dioxus counter: blaze compiles the app in the page and links it
into a module rustc built that holds `std`, Dioxus and the wasm-bindgen glue.

The work went in two passes, and the report keeps both. Sections 1 to 4 were written after
the first pass (one canvas demo, compiler and output both wasm64) and their measurements are
of that state. Sections 5 to 7 cover what came after: wasm32, more demos, Dioxus, proc
macros, the playground, and stable Rust 1.99. Where the two disagree, the later sections
are current. "What does not work" and the list of next steps at the end are current.

Everything below is marked as measured, read from code, or inferred. Timings are from one
Linux VM; where a number comes from a single run it says so.

## How to run it

    cargo install wasm-bindgen-cli --version 0.2.122
    crates/blaze-wasm/web/build.sh              # the compiler, and the source of core (from library/)
    crates/blaze-wasm/web/build-dioxus.sh       # the page itself and the packed Dioxus workspace, needs clang
    python3 -m http.server -d crates/blaze-wasm/web 8377

Then open `http://localhost:8377/` in Chrome. The line at the bottom shows what the last
build did. `node crates/blaze-wasm/web/check.mjs` builds and edits the canvas programs
without a browser and asserts on what they draw.

`wasm-bindgen` has to be the version in `web/dioxus/Cargo.lock`. A newer one refuses the
module, and 0.2.129 also emits promise-integration code that needs `exnref`, which the
Chrome on this VM does not have.

I ran it in the Chrome on this VM and in Node 24. I did not try Firefox or Safari.

## What was built

- `crates/hir-ty/src/mir/codegen/wasm.rs` (about 2,200 lines, behind the `wasm` feature of
  `hir-ty`): lowers one function of Cranelift IR to a WebAssembly function body.
- `crates/blaze-wasm`: a `Compiler` that holds a salsa database over an in-memory file set,
  a linker that turns the code generator's objects into a WebAssembly module (`link.rs`),
  the reading of a module rustc built and of rustc's symbol names (`host.rs`, `names.rs`),
  the packing of a cargo workspace (`pack.rs`), the proc macros that run in the compiler
  (`macros.rs`, `core-macro/`), a small C ABI for the page, and a CLI that does the packing
  natively.
- `crates/blaze-wasm/web`: the playground (`dioxus/playground`, a Dioxus app), the JavaScript it
  leaves the compiler to (`playground.js`, `worker.js`, `compiler.js`), the runtime that
  holds a program's memory and function table (`program.js`), the canvas programs
  (`demos/`), and the workspace of the Dioxus demo (`dioxus/`).
- Small portability fixes in `stdx`, `paths` and `ide-db`, listed under "Blockers".

`blaze-wasm` is a workspace of its own, outside the main one. The `wasm` feature changes
what `codegen.rs` emits, and inside the main workspace cargo would unify that feature into
native `blaze`. With the feature off, `codegen.rs` compiles as before apart from
`std::time::Instant` becoming `stdx::time::Instant`, which is a re-export on native.

## 1. Does the front end run on wasm?

(First pass. The compiler is wasm32 now, see section 5.)

Yes.

Measured: `cargo check --target wasm32-unknown-unknown` passes for `hir-ty` and `ide`
without changes, including Cranelift as a dependency. The whole stack used here (`base-db`,
`hir-def`, `hir-ty`, `hir`, `ide-db`, MIR lowering, monomorphization, `codegen.rs`) builds
for `wasm64-unknown-unknown` and runs in Node and in Chrome. An earlier `wasm32-wasip1`
build also ran a small program in Node before I moved to wasm64; that path is not in the
branch. It name-resolves and type-checks `core` and the demo, lowers them to MIR and
generates code.

### Blockers hit, and what was done

- **Threads.** rust-analyzer drops syntax trees on a background thread, and spawning it
  fails on wasm. The repository already has a `no_salsa_async_drops` cfg for this;
  `.cargo/config.toml` now sets it for `wasm64-unknown-unknown`. Nothing else in the path
  needed a thread. Salsa itself ran fine on one thread. `codegen.rs` asks its
  `SymbolResolver` to run work in parallel; the default implementation does nothing, so
  codegen is serial here.
- **Clock.** `std::time::Instant::now()` panics on `wasm*-unknown-unknown`. `codegen.rs`
  and `ide-db` time their phases with it. Added `stdx::time::Instant`, which is the std one
  natively and reads an imported `blaze.now` on wasm.
- **`std::process`.** `stdx::process` already had a wasm stub, but gated on
  `target_arch = "wasm32"`. Changed to `target_family = "wasm"`.
- **Paths.** `AbsPathBuf::assert("/")` panics on `wasm*-unknown-unknown` because
  `Path::is_absolute` is false for everything there. `paths` now treats a path with a root
  as absolute on that target.
- **Filesystem and VFS.** Not a blocker. The `vfs` crate's `FileSet` and `VfsPath` take
  virtual paths; `vfs-notify` and the loader are not used. Files are set with
  `Compiler::set_file`.
- **Cargo metadata and project loading.** Bypassed, not solved. `project-model` is not
  compiled in; `blaze-wasm` builds a two-crate graph by hand (the program and `core`) with
  cfgs and a data layout for wasm64. A real project would need its crate graph computed
  somewhere else (a server, or ahead of time) and shipped as data.
- **Sysroot.** The sources of `core` (plus the `core_arch` and `core_simd` files it
  includes) are packed into one 6.7 MiB file by `blaze-wasm bundle` and loaded into the
  in-memory file set. `alloc` and `std` are not loaded.
- **Proc macros.** None are loaded; `core` needs none since its derives are built in. The
  proc-macro server loads native dylibs, which cannot work in wasm. The options I see are
  proc macros compiled to wasm and run as separate modules (this needs a wasm proc-macro
  ABI and a build of each macro crate, done outside the browser), or expanding ahead of
  time on a server. Neither was tried.
- **Build scripts.** Not run and cannot be. Their outputs would have to be shipped.
- **Memory.** Measured: the compiler's linear memory is 252 MiB after building the demo.
  Inferred: that is mostly `core`. Inferred: wasm32 caps a page at 4 GB, so a program that
  takes 7 GB natively (the dx-sized daemon) does not fit in wasm32. wasm64 lifts the
  address limit, but browsers still cap memory64 memories; I did not measure where Chrome
  stops. A realistic size for now is a small app plus `core`/`alloc` and a few
  dependencies, not a large workspace.

### Why wasm64

(First pass. Both are wasm32 now, see section 5.)

Two things in blaze assume the target's pointer is as wide as the compiler's own:
`codegen.rs` builds IR with `I64` pointers throughout, and the MIR interpreter, which
evaluates constants, refuses a target whose pointer size differs from the host's
(`mir/eval.rs`: "targets with different pointer size from host"). Compiling for wasm64
with a wasm64 build of the compiler satisfies both without touching either. The cost is
that memory64 is required and wasm64 is a tier 3 Rust target (`-Zbuild-std`).

Getting to wasm32 means parameterising `codegen.rs` on pointer width and letting the
interpreter run a 32-bit target on any host. That is mechanical but wide.

### Prior art

`rust-analyzer/rust-analyzer-wasm` ran the IDE half of rust-analyzer in a page, with a
packed copy of the standard library sources (its `rust-pack` directory). Checked through
the GitHub API: the repository is archived and its last push was 2022-06-14, so it is four
years stale. I did not build or read it beyond that. It is evidence that the analysis stack has run in a browser before, and nothing
more; it did no code generation. I did not reuse anything from it.

## 2. What does codegen produce in a browser?

Read from the Cranelift 0.136 sources that `hir-ty` depends on: `cranelift-codegen/src/isa`
has `x64`, `aarch64`, `riscv64`, `s390x`, `pulley32` and `pulley64`. There is no
WebAssembly backend.

### Options

**Wasm lowered from Cranelift IR (what this branch does).** `codegen.rs` builds Cranelift
IR as it does natively, and `wasm.rs` writes a wasm function from that IR instead of
calling a native ISA.

- Carried over: all of `codegen.rs` (9,200 lines), with one `emit` function swapped by
  cfg. Monomorphization, vtables, statics, drop glue, the object writer and the patcher
  that decides what an edit touched are used as they are.
- Speed: not measured against anything. Inferred: fair but not good. SSA values become
  wasm locals, stack slots live in linear memory, control flow is nested blocks with a
  dispatch loop for backward jumps, and every call is a `call_indirect`. The browser's
  optimising tier recovers some of it.
- Hot reload: every function has a slot in one `WebAssembly.Table`, every call goes
  through the table, and a function's address is its slot. A patch is a small module that
  imports the memory, the table and the stack pointer, and whose element segment writes
  the new functions into their slots. Statics keep their addresses and contents.
- ABI: Cranelift signatures map to wasm signatures directly; `i128` is two `i64`. Calls to
  functions nothing defines become imports from the page, which is how the demo draws.
- Not done: unwinding (a panic that would unwind aborts), SIMD types, and whatever opcodes
  the demo and its slice of `core` never reached.

**Wasm emitted straight from MIR.** A new backend beside `codegen.rs`. It would produce
better wasm (structured control flow from MIR, direct calls where a function cannot
change) but shares none of `codegen.rs`: layouts, ABI, intrinsics, vtables, drop glue and
the patcher's bookkeeping would all be written again. Hot reload and ABI as above.
Inferred: months, against the two thousand lines the IR route took.

**Pulley.** Cranelift can emit Pulley bytecode, and that carries `codegen.rs` over
completely. But the page then needs a Pulley interpreter, and what runs is interpreted
bytecode inside wasm. Not tried. Inferred: several times slower than emitted wasm, and
calling into the page (DOM, canvas) needs a trampoline per signature. Hot reload is easy
because code is data. I see no reason to pick it over emitted wasm now that emitted wasm
works.

**The MIR interpreter.** Nearly free and already runs in wasm; it evaluates the demo's
constants here. It is slow, it only runs a target with the host's pointer width, and it
has no path to calling host functions. Useful as a fallback for a function the code
generator rejects, not as the engine.

**Native code from Cranelift inside wasm.** Useless in a browser. Cranelift will produce
x86-64 bytes there, and nothing can execute them.

### Recommendation

Keep lowering Cranelift IR to wasm. It reuses everything, it works today, and the two
obvious speed problems (indirect calls everywhere, locals for every value) can be fixed
inside `wasm.rs` and the linker later. Revisit a MIR backend only if the generated code
turns out too slow for real UI work, which nothing here measured.

## 3. Where do std and dependencies come from?

For this demo, `core` is compiled from source by blaze in the page. Measured: the first
module has 271 functions: 8 from the demo, 59 non-generic functions of `core`, and 204
instances of generic ones. One function of `core` that the demo reaches was not compiled
(`core::fmt::num::imp::_fmt_inner`, integer formatting, reached only through panic
messages); it becomes an import that throws if called. I did not look into why.

That is a narrow slice. Formatting, `alloc`, collections and `std` were not attempted in
wasm, so I cannot say how much of them blaze's codegen handles on this target. Natively
blaze leans on rustc's copies for what it cannot compile; in the browser there is no such
fallback, so a function that fails to compile is simply missing.

The other route is to ship `core`, `alloc`, `std` and dependencies as wasm built by rustc
ahead of time, and link the program against them in the page. Inferred, not tried: the
linker here is about 500 lines because it only handles objects that blaze wrote; linking
rustc's wasm objects means reading the wasm object format and its relocations, and agreeing
on an ABI. rustc targets wasm32 by default while this spike is wasm64, and blaze's "every
call through the table" convention has to meet rustc's direct calls. A wasm build of
`wasm-ld` would do the format work but not the hot-swap part.

For a first real demo I would stay with source: add `alloc` (with a bump or dlmalloc
allocator written in Rust) and compile it the same way as `core`, and find out what breaks.
Prebuilt objects become worth it when first-build time on a large dependency tree hurts.

## 4. The prototype

All four steps work:

a. the analysis stack as wasm, in a page, type-checking a single-file program from memory;
b. MIR for that program, produced in wasm;
c. its functions run in the page as emitted wasm;
d. an edit recompiles only what changed and swaps it in without a reload.

The demo (now `crates/blaze-wasm/web/demos/balls.rs`) is `#![no_std]`: bouncing balls drawn through
four functions the page provides (`fill`, `rect`, `circle`, `text`). It uses a trait, an
`impl`, slices, iterators, `f64::clamp`, tuples, `match`, overflow checks and a
`static mut`. It is not web-sys and not a UI framework; both need `alloc`, `std` and proc
macros.

`check.mjs` asserts the behaviour that matters: after changing a constant only the
functions using it are compiled again, after changing a string only `frame` is, and a ball
dropped before the edits is still there afterwards.

### Measurements

Compiler and sources (measured, sizes in bytes):

| | raw | gzip -9 | brotli q9 |
|-|-|-|-|
| `blaze_wasm.wasm` | 23,566,999 | 4,543,855 | 3,403,701 |
| `core.bundle` | 7,003,958 | 1,354,134 | 1,053,260 |

The compiler is a plain `--release` build. No `wasm-opt`, no `opt-level = "z"`, no LTO.

Load and first build in Chrome, served from localhost (three page loads):

| | |
|-|-|
| fetch compiler and sources | 77 to 106 ms |
| instantiate compiler, load sources | 77 to 95 ms |
| first build, total | 4.5, 5.1 and 7.2 s |
| of which analysis (listing the functions, which runs name resolution over `core`) | 2.9 to 4.9 s |
| of which codegen (type inference, MIR, monomorphization, emitting 271 functions) | 1.5 to 2.3 s |
| of which link | 5 to 12 ms |
| first module | 143,980 bytes |
| compiler memory after the first build | 252 MiB |

Edits in Chrome after the first build. "Edit to running" is from the page handing the
source to the worker until the new functions are in the table; it leaves out the 60 ms the
page waits for typing to pause. Each row is one edit; the ranges are over the two or three
times I made that edit in different page loads:

| edit | functions swapped | module | codegen | edit to running |
|-|-|-|-|-|
| change `RADIUS` | 2 | 2,922 B | 11 to 49 ms | 16 to 204 ms |
| change the label string | 1 | 2,399 B | 20 ms | 24 ms (single run) |
| change `GRAVITY` | 1 | 2,307 B | 9.6 ms | 17 ms (single run) |
| change `RADIUS` and the label together | 3 | 5,233 B | 64 ms | 227 ms (single run) |

Analysis on an edit took 1 to 7 ms and link 1 to 3 ms in every run. The slow end of each
range is the first edit after the first build.

In Node 24, five runs of `check.mjs`: first build 6.3 to 8.3 s, the `RADIUS` edit 72 to
223 ms, the label edit 11 to 31 ms.

Per-function compile time, inferred from the above: 6 to 8 ms per function averaged over
the first build, most of it front-end work on `core` that is then cached; 10 to 30 ms per
function on a warm edit, where the cost is re-checking what the edit could have touched
rather than emitting code.

The first build varies between runs by more than I can explain (4.5 to 7.2 s in Chrome on
an otherwise idle machine, 11 s once while the machine was compiling). Treat it as rough.

## 5. wasm32, and more programs

Both the compiler and what it emits are wasm32 now, so memory64 is no longer needed.

- **Emitted code.** `codegen.rs` takes its pointer type and word size from the `wasm`
  feature (`PTR`, `WORD`). What had to change were the places that had the width written
  in: fat pointers and vtable receivers assumed 16 bytes, `memcpy` and friends were
  declared with 64-bit sizes, and a byte string literal, which rust-analyzer's MIR stores as
  pointer and length even when its type is a thin `&[u8; N]`, was only recognised at 64
  bits. The last one left `f64::clamp` and the bounds-check panics uncompiled, and the demo
  trapped when a ball first hit a wall.
- **The compiler.** Builds for `wasm32-unknown-unknown` on stable with no `-Zbuild-std`.
  The MIR interpreter's refusal of a target with another pointer width does not come up,
  because the host is now as wide as the target.
- **Cranelift.** There is still no wasm ISA. `wasm.rs` writes wasm from the IR and only
  borrows the settings of the x86-64 ISA, so `Target::host()` names that one when the
  `wasm` feature is on.

The canvas programs are in `web/demos`: bouncing balls, a fountain of drops whose
constants are meant to be edited while it runs, the game of life (2D arrays,
by-value array iterators), sorting (`dyn Sorter`, slices) and the Mandelbrot set (operator
overloading, closures, `Option`). Measured in Chrome, one run each, time from picking the
program to it running, with the compiler already loaded: balls 3.2 s (747 functions in a
681 kB module), sort 2.4 s (397 functions), life 2.4 to 2.7 s (404), mandelbrot 2.1 to
2.4 s (43). About 2 s of each is name resolution over `core`, which is repeated because
picking another canvas program starts a fresh compiler. Compiler memory is 180 to 210 MiB.
A `RADIUS` edit in balls: 71 ms, 2 functions. A `WIND` edit in the fountain: 72 ms,
1 function.

## 6. Dioxus: linking into a module rustc built

The Dioxus demo answers section 3 the other way round from what it recommended: `std` and
the dependencies are not compiled in the page. They come from rustc.

**The host module.** `web/dioxus` is a cargo workspace with two crates: `app`, the counter
the editor shows, and `playground`, the page itself (section 7), a staticlib that depends
on Dioxus and exports `blaze_launch(app)`, which takes the table slot of the program's
`App`, and `blaze_rerender()`. `build-dioxus.sh` builds `playground` with rustc and links
it with `rust-lld` itself, with `--whole-archive --export-all --no-gc-sections
--export-table --growable-table` and an exported `__stack_pointer`, then runs
`wasm-bindgen --keep-lld-exports` over it. This is the same shape as the fat module
subsecond builds for web hot-patching. Measured: 15.9 MB of wasm, 16,381 functions and
19,265 data symbols that the program can link against by their rustc v0 symbol names.
Until the playground became the host, the host was a crate of its own with only Dioxus in
it, 7.3 MB.

**The workspace.** blaze in the page still has to analyse everything the app's types come
from. `blaze-wasm pack` loads the cargo workspace natively and writes one file with the
sources of all 206 crates (3,392 files, `std` included), the crate graph, and every proc
macro expansion rust-analyzer made while analysing the app (652 of them). Measured: 74 MB
as JSON, not compressed. In the page, name resolution over it takes 8 to 12 s and type
inference of the app 3 to 6 s, and the compiler's memory is about 1 GiB.

**The link.** blaze compiles the functions of `app` and the generic instances they need.
Everything else it calls resolves against the host module's exports: functions by symbol
name with the wasm signatures compared (measured: 83 host functions called by the counter,
no signature differing), statics through exported globals. The patch module is
instantiated in the host's memory and table, in table slots and memory the page sets aside
after the host's own. The host only ever reaches the program through function pointers
(`launch(App)`, event closures), and a function pointer is a table slot, so overwriting a
slot swaps the function. After a swap the page calls `blaze_rerender`, which marks every
scope dirty. No jump table like `subsecond::call` was needed.

Measured in Chrome, one run: first build 2.2 s (1,766 functions, 1.0 MB module); changing
the heading text 95 ms (2 functions, 10 kB); adding a component with props 170 ms (30
functions). The count survives every edit. An edit that does not compile leaves the old
app running and fixing it recovers.

**Matching rustc.** The program and the host must agree on layouts, symbol names, the call
ABI, vtables and `TypeId`s. The last one bit: with the host on stable 1.99 and blaze
reproducing the `TypeId` hashes of an older nightly, Dioxus's `downcast` of an event failed
and a click panicked. The `blaze-spike` branch has since pinned blaze to stable 1.99.0 with
its own copy of `rustc_abi` and fixed the hashing, this branch has that merged, and the
host is built with 1.99.0 (`web/dioxus/rust-toolchain.toml`). Clicks work.

**Bugs this found**, all fixed: an empty slice constant sharing its address with the next
constant, which was then emitted without its pointers relocated (static text in an `if`
rendered empty); a recompiled closure getting a fresh table slot rather than its old one,
so unchanged code called the stale closure; and a panic in the linker after an edit that
failed to compile.

**Proc macros.** No proc-macro server runs in the page. The expansions recorded in the
pack cover the source as it was packed. For a call the pack has no expansion of, which is
any macro call the user edits, the compiler runs the macro itself: `rsx!` through the
`dioxus-rsx` crate, which is an ordinary library, and `#[component]` and `#[derive(Props)]`
through the source of `dioxus-core-macro`, which `core-macro/build.rs` copies into a
library crate (a proc-macro crate cannot be linked) with `syn::parse` turned into
`syn::parse2`. That is three macros, picked by name in `macros.rs`. It is not general: any
other macro only works where its recorded expansion still applies. The general route,
not tried, is each proc-macro crate built to its own wasm module with the compiler as the
bridge server, as rust-analyzer's proc-macro server is for dylibs.

## 7. The playground

The page is one Dioxus app built by rustc (`web/dioxus/playground`): the list of programs
down the left, the editor (`dioxus-code-editor`, highlighted by `dioxus-code`), the light
and dark themes, the output pane and, under both, a log with a line for each build, which
starts with how long the build took. It compiles nothing. It tells `playground.js` which
program to show and of every edit, and is told how each build went. JavaScript keeps the
compiler workers, the instantiation of what they emit and the canvas functions. There is
one worker for the canvas programs and one for the Dioxus workspace, each started when
first needed and kept, so going from the counter to a canvas program and back keeps the
counter's state.

The playground is also the module the counter is linked into, so the page has one Dioxus.
`blaze_launch` gets the table slot of the `App` blaze compiled, and the playground renders
it in the output pane as a component of its own VirtualDom; after a swap `blaze_rerender`
marks every scope dirty. Checked in Chrome, one run: clicks, an edited heading, a new
`#[component]` with props, an edit that does not compile and its fix, and going to canvas
programs and back with the count kept.

An earlier state had two Dioxus instances, the playground and a separate host module
mounted in its output pane. Both find the element an event is for by a number on it, so a
click on the counter's button reached the playground as a click on its theme button.

What unifying costs:

- The page is 15.9 MB of wasm, since nothing of it can be removed, where the playground
  alone was 3.3 MB and the separate host 7.3 MB.
- It cannot be built with `dx`, which does not link this way. The editor's style sheets are
  `asset!`s that only `dx` resolves, so `build-dioxus.sh` copies them into `web/assets` for
  `index.html` to load, and puts an empty sheet under the placeholder name the editor asks
  for instead.
- A panic in the program is a panic in the page. Not tried: what the page does after one.

## What does not work

- Dependencies other than the ones in the Dioxus workspace. A project is packed natively,
  ahead of time; nothing in the page runs cargo, a build script or a proc-macro crate.
- Proc macros beyond `rsx!`, `#[component]` and `#[derive(Props)]`, once the call is edited.
- Compile errors are not reported as diagnostics. The page lists the functions that could
  not be built with a reason from MIR lowering or codegen ("not compiled: App: frontend:
  mir: HasErrors"), the old code keeps running, and fixing the error recovers.
- A panic in the compiler ends it. The compiler is built without unwinding, and native
  blaze relies on catching a panic to find out that an edit took away something the last
  build knew. Here a build that finds a function gone is a full build instead of a patch,
  which covers deleting or renaming a function. Other ways of invalidating what the last
  build held (I did not find one) would trap, and the page then needs a reload.
- Panics in the program abort; there is no unwinding. A trap stops a canvas program until
  the next build. The canvas programs have no `#[panic_handler]`.
- Functions declared inside a function body are not listed as roots.
- A changed `static` gets a fresh address and its initial value; the old one is leaked.
  Table slots and data are never reclaimed, and the Dioxus program has a fixed allowance
  (131,072 slots, 64 MiB).
- Layout changes to live data are not handled.
- Hooks are not tracked. Adding or reordering hooks in a running component is whatever
  Dioxus does with a hook list that no longer matches.
- Tests: `blaze-wasm` has unit tests for the macros only, and `check.mjs` covers the canvas
  programs. The linker, the Dioxus demo and the playground are only tested by hand, in
  Chrome. With the `wasm` feature on, `hir-ty` has dead-code warnings for the native emit
  path.
- Startup of the Dioxus demo: 74 MB to fetch and 11 to 18 s of analysis before the first
  build.

## From here to a real hot-reloading compiler in the browser

Sizes are rough guesses at effort, not measurements.

1. **Startup and memory for a real workspace** (weeks). 74 MB of sources, 11 to 18 s of
   analysis and 1 GiB for a counter. Most of the analysis is of crates that never change:
   it needs a persisted database or a prebuilt snapshot, and a compressed pack.
2. **Proc macros in general** (weeks). Macro crates built to wasm at pack time, and the
   compiler as their bridge server.
3. **Diagnostics in the page** (days). rust-analyzer already computes them; they need
   plumbing through the ABI and an editor that can show them.
4. **The playground as the host** (days to a week). One Dioxus in the page instead of two.
5. **Unwinding in the compiler** (days). Either a build with `panic=unwind` on wasm
   exception handling, or the patcher asking whether a thing still exists before it asks
   about it.
6. **A crate graph computed for any project** (1 to 2 weeks). `pack` does this natively for
   one workspace; a service would do it for a project the user brings.
7. **Code quality** (1 to 2 weeks for the first round). Direct calls to functions that
   have not changed, structured control flow, fewer locals.
8. **Unwinding in the program** (1 to 2 weeks). The wasm exception-handling proposal maps
   onto `try_call`'s exceptional edges, and has to match what rustc's module does.
9. **Reclaiming slots and data, and state migration on layout changes** (1 to 2 weeks for
   the first, open-ended for the second).
