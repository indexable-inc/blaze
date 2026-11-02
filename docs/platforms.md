---
status: record
summary: The state of each platform under blaze v1 (rustc's link line, the splice, `blaze-engine weaken`), from v1's last CI run that covered them all. v2 has not been run on any platform yet.
date: 2026-10-30
---
# Platforms

This is the state of each platform blaze v1 was run on, as of its integration into v2 on
2026-10-30. v1's link (rustc's link line with blaze's object first), its splice
(`crates/blaze-splice`), `blaze-engine weaken`, `blaze-live` as described here and
`BLAZE_STD_FROM` are gone; v2 links with its own linker (`crates/blaze-link`,
[v2/plan/lanes/link.md](v2/plan/lanes/link.md)), and no v2 build has been run on any platform
in CI yet. The
numbers are from GitHub Actions run 39057174862 on commit 888e87df74, the last recorded run
that covered all four native platforms under both backends, unless a line says otherwise.
The report behind it is [reports/PLAT-ABI2.md](reports/PLAT-ABI2.md). The table of what is
supported where is in [support.md](support.md); this page says how each port works and what
is specific to it.

The earlier per-platform reports (first ports, measured on a nightly toolchain, and the ABI
rounds that followed) are superseded by this page and are kept on the development branch
only.

## What is common

The target is an input of the process: `--target <triple>`, else the host. One process has
one target. Everything that differs between platforms asks the `Target` in `target.rs` for
the architecture, object format, calling convention and unwind format.

Three things are implemented once for all platforms and were the subject of the last
round:

- `sym` operands in `naked_asm!` are one MIR operand and one table of relocations with rows
  for ELF x86-64, ELF AArch64, COFF x86-64 and Mach-O arm64.
- A 128-bit integer or an aggregate aligned to 16 bytes that does not fit the remaining
  registers goes to the stack whole, for AAPCS64 and Apple alike. On x86-64 an aggregate
  aligned to 16 bytes on the stack is padded where needed; before that fix it could land
  eight bytes off.
- C-variadic calls and definitions have one model with three conventions: System V,
  Windows, and Apple.

The daemon's full link keeps the code nothing in the executable reaches, on every platform:
rustc's line has the linker take it out (`--gc-sections` for ELF, `-dead_strip` for Apple's
linker, `/OPT:REF` for `link.exe`, which becomes `/OPT:NOREF`), and a later patch may call
it, for example an instance of a generic function from another crate for a type an edit
starts using. A splice that does not find what the patch calls declines it ("... is not in
the executable") and the edit is linked in full. When the link that keeps it fails on names
nothing defines that only other objects use (zed's webrtc calls X11's `libXtst`, which
rustc's line never links, from code it takes out), the daemon links as rustc's line has it,
from the response file `link.reached.args` next to its own, and links that executable so
from then on. `unreached.py` tests this on Linux and
macOS; on Linux at 3a844b09d5, before the rule covered more than macOS, it was 1 of 3. On
Windows only the link line is tested, by a unit test in `cargo/plan.rs`. rustc's own link,
which the first edits after a cold build are spliced into, still takes the code out, and
those edits are linked in full until step G of [no-rustc.md](no-rustc.md) links nothing
rustc built ([#52](https://github.com/indexable-inc/blaze/issues/52)). What that link keeps
includes the standard library's optimized code, where a function an object keeps to itself
can take its arguments otherwise than its signature says: LLVM passes the `&u16` of
`<u16 as Debug>::fmt` as a `u16`. A patch calls a local of rustc's executable only if it
was a hidden global or is of a crate that calls by symbol; blaze builds the rest into it.

On every platform the gate in CI is programs, stress, unwinds, splices, asmedits, includes,
worktrees, keys, storekills, storecorrupts and storeshared, under Cranelift and under LLVM at
`-O2` with fast instruction selection. Until 2026-10-30 it also compared a corpus of
functions, their type layouts and a set of breakers with rustc's build; the last counts of
that corpus were:

| Platform | Corpus functions from blaze, both backends | Layouts that differ | LLVM compiled itself |
|---|---|---|---|
| Linux x86-64 | 822 of 822 | 0 of 893 | 11,787 of 11,787 |
| Linux AArch64, native | 825 of 825 | 0 of 903 | 11,813 of 11,813 |
| Windows x86-64 MSVC | 830 of 830 | 0 of 898 | 11,801 of 11,801 |
| macOS arm64 | 828 of 828 | 0 of 914 | 11,879 of 11,879 |

The counts differ because some corpus cases are for one architecture or one operating
system.

## Linux x86-64

The platform the work was done on, and the only one where everything has been run: the
whole gate, `debuginfo.py`, the build-model cases (deleted 2026-10-30), `storefull.py`,
`remote.py` against MinIO, the fuzzer, rustc's own test suites, the crate sweep and `blaze`. The gate was rerun for
these documents at f191c6e42a under both backends; see [results.md](results.md).

Objects are ELF. A full link runs rustc's link line with blaze's object first and
`--allow-multiple-definition`, so the first definition wins. The splice appends segments to
the executable and rewrites the debug sections so that a debugger sees the new code. About
60 of the 71 steps of `splices.py` are spliced; the others link in full by design. An edit
made while the program is running is linked in full, because the kernel refuses the write.

## Linux AArch64

Runs natively on `ubuntu-24.04-arm` in CI under both backends, including `debuginfo.py`
(under Cranelift gdb prints 9 of 9 locals; under LLVM no locals are described),
the build-model cases (204 of 204 when last run, four rows that need an x86-64 host not
run), `storefull.py`
and `farsplices.py`.

A `b` or `bl` reaches 128 MB. When a function and its replacement are further apart than
that, the splice uses a longer jump of 12 bytes, and blaze makes no function shorter than
that. `farsplices.py` tests this with 160 MB between the code and the splice, and
`bigbinary.py` with 220 MB of code.

An executable marked as needing BTI landing pads is declined by the splice. That check is
written and has never been exercised: no test builds such an executable.

AArch64 can also be a cross target from Linux x86-64, with the programs run under
`qemu-aarch64`. The counts are the same as native except `debuginfo.py`, which does not
pass under qemu because the harness runs gdb and backtraces without the emulator.

The fuzzer has not been run on AArch64, and nobody has recorded running `blaze`
there.

## Windows x86-64 (MSVC)

Runs on `windows-latest` in CI under both backends. Objects are COFF and link with MSVC
`link.exe` using rustc's line, with `/FORCE:MULTIPLE` so that blaze's definitions win.

What is specific to the port:

- Unwinding. Windows unwinds in two passes and expects landing pads to be separate
  functions. Cranelift cannot make a call that names its landing pad for the Windows
  calling convention, so a call with a pad is a plain call followed by a look at a slot of
  the frame, and each object carries a personality function of its own (`unwind/seh.rs`).
  This costs a load and a branch after every such call. Exceptions that are not C++
  exceptions (structured exceptions, foreign unwinds) pass through blaze's frames without
  running cleanup; that is inferred from the code and not tested. A Rust panic is a C++
  exception on MSVC and is handled.
- The splice (`splice/pe.rs`) adds sections behind the linker's last. `splices.py` is 71 of
  71 with 57 spliced, 13 full links and 1 step with nothing edited, and fails on Windows if the
  count is not 57 (train run 40494685771, both backends). Five of the full links are the steps
  after a thread-local's first value is edited, which then carry its own copy
  ([live.md](live.md#statics)); Linux and macOS link those five in full too. COFF
  has no symbol sizes, so each named static gets a section of its own, and a static that
  grows is not spliced. Thread-local data in a patch declines the splice. Executables with
  Control Flow Guard, a signature, or no PDB always take the full link (inferred from the
  checks in the code; the harness links none of these).
- A running `.exe` cannot be written, even by the linker. The daemon renames it aside and
  links a new file, or splices a copy and puts it in its place.
- Live edits (`blaze-live`): the program is named by the GUID of its PDB, reserves room
  within 2 GB behind its image with `VirtualAlloc`, and registers each patch's `.pdata` with
  `RtlAddFunctionTable`, so a panic unwinds through patched code. The daemon wakes it through a
  named event where Unix has a datagram socket. A static with another value asks for a restart
  (the splice writes it in place). `blaze_live::edit`, which the demo's editor sends each
  keystroke with, talks to the daemon's AF_UNIX socket through Winsock. `harness/live_e2e.py`
  passes, by hand, not in CI; life's latency is in [live.md](live.md#latency-under-typing).
- The daemon's socket is an AF_UNIX socket. The check that only the user can reach it is
  not implemented on Windows.

Debug info is CodeView procedures and line tables: backtraces and breakpoints by line work,
a debugger shows no types, parameters or locals. The PDB is stale after a splice until the
next full link. `blaze` builds through the daemon (`[blaze]`) and links with `rust-lld`
as the repository's cargo configuration names it. That was run by hand on one machine, on a toy
crate and `demo/life`, not in CI; `harness/blaze_cli.py` and the fuzzer are not run there.
The GNU and ARM64 Windows targets have nothing implemented.

## macOS arm64

Runs on `macos-latest` in CI under both backends, with `worktrees.py` in all three of its
modes. Objects are Mach-O.

What is specific to the port:

- Linking. Apple's linker has no flag to keep the first of two definitions. `blaze-engine weaken`
  makes copies of rustc's objects and archives whose definitions are weak, and blaze's
  object is linked ahead of them.
- The splice (`splice/macho.rs`). The loader wants `__LINKEDIT` last with the signature at
  its end. The patch goes where `__LINKEDIT` was, in two new segments, `__LINKEDIT` is
  written again behind them, and the ad hoc signature is redone. The result is written to a
  copy that replaces the executable by rename. dyld on macOS 15.4 to 15.7 finds
  `__unwind_info` and `__eh_frame` only in the segment named `__TEXT`, so `__unwind_info`
  moves into the header's spare room and the patch's frames take its place in front of the
  program's. When the patch's frames are larger than the program's unwind table, both tables
  move into that room, the program's frames where the table has them and the patch's behind;
  only a patch whose frames don't fit there either is linked in full. The room for two more load commands and the table comes from `-Wl,-headerpad`,
  which `blaze` gives the link in a file of its own beside the executable
  (`bin.headerpad`), last on the line: 1 MiB, or what the line asks for. The table grows
  with the program (3 MB for a GUI app of 570 crates). Before a link the daemon makes the
  room in that file what the executable there wants, and at least what the unwind
  information of the objects it links will take: their `__eh_frame` and a table entry for
  each frame and each `__compact_unwind` entry, counted from the objects and the prebuilt
  standard library, so a first link has the room too (#173: bun's base was linked twice,
  260 s each). After each full link it reads the header, and when the room is still short
  it writes what the table needs and a quarter more into that file and links once again; the
  links after start from it. A link line
  given to the daemon by hand keeps the headerpad it has, and without enough the splice
  declines. The file does not grow from splice to splice.
- The splice reads and writes the loader's fixups as dyld info, the tables rustc's default
  deployment target of 11.0 gets. For a target of 12 or later (nix's shell sets 14.0) Apple's
  linker writes them as chains instead, so the daemon's link line has `-Wl,-no_fixup_chains`.
  An executable linked with chained fixups anyway, by a line given to the daemon by hand, is
  declined and linked in full.
- The calling convention is Apple's variant of AAPCS64, including its rule that variadic
  arguments always go on the stack.

Not there: debug info is not checked on macOS (the harness reads DWARF out of ELF).
`blaze` serves dev and test builds;
`harness/blaze_cli.py --small` (ripgrep, fd, hyperfine) passes under both backends, and
the larger projects of that harness have not been run ([cargo.md](cargo.md#macos-arm64)).
`storefull.py`, `remote.py`, `farsplices.py` and the fuzzer are not run. The first port
noted that under LLVM the unwind rules are not exact inside prologues and epilogues;
nothing later says that changed.

`x86_64-apple-darwin` is refused. With the triple forced through, blaze compiled the corpus
and the link failed on a relocation in the unwind table; the splice and the LLVM object
reader are arm64 only as well.

## wasm32

blaze runs natively and emits wasm32 with its own lowering from Cranelift IR, written as a
relocatable wasm object: `v128` with the simd128 and relaxed-simd operations, 128-bit
integers, the wasm C ABI (vectors as `v128`, an aggregate of one scalar as that scalar,
C-variadic calls and definitions through a buffer), `#[link_section]` statics as custom
sections and a `target_features` section, as LLVM writes them. In the harnesses `rust-lld`
links these objects ahead of rustc's rlibs. With nothing of rustc's (the whole build of
[no-rustc.md](no-rustc.md), `BLAZE_STD_FROM=off`), blaze compiles the target's standard
library as a unit of its own and links with `rust-lld -flavor wasm` (`link.rs`), with the
toolchain's wasi-libc for `wasm32-wasip1`. A `#![no_std]` program's unit is `core`,
`compiler_builtins` and what it names with `extern crate`, as rustc puts them in, so a
guest's own `#[panic_handler]` is the only one (#622). The splice writes the module again
with new function bodies under the same indices.

Measured 2026-10-27 on arm64 ix VMs with the pinned nightly (`nightly-2026-10-25`) and
wasmtime 49.0.2, at main fdc1c53f15 (#622 for the `no_std` rows):

| What | Result |
|---|---|
| `wasm32-wasip1` corpus | MATCH, 957 of 959 functions from blaze |
| the same with `+simd128,+relaxed-simd` | MATCH, 964 of 966 |
| stress, splices | 100 of 100, 71 of 71 |
| `allblaze_wasm.py`, nothing of rustc's | 3 wasip1 programs, a wasm32-unknown-unknown cdylib and a `#![no_std]` simd128 guest, each as cargo's build answers |
| blaze-manifest + blaze-resolve over blaze's workspace and lock file (issue #458), nothing of rustc's | 45,837 functions, built and linked in 9.1 s (3.2 GB), output byte-identical to rustc's native build; 36.6 MB vs 3.9 MB, 0.30 vs 0.03 s a run |
| the same with `blaze build --release --target wasm32-wasip1`, a store that starts empty | 64.5 s (9.6 GB at `CARGO_BUILD_JOBS=2`), its build scripts, 157 macro expansions (blaze's proc-macro server) and std unit blaze's; output byte-identical; 0.06 s again with nothing changed |
| indexable-inc/wasm-host guests (`no_std`, `+simd128,+relaxed-simd,+wide-arithmetic,+tail-call`), nothing of rustc's | request transcripts byte-identical to rustc's; bench_raw 534,750 B vs 10,757 B, 833 to 836 vs 240 to 242 ns a request; bench_worker 984,365 B vs 49,930 B, 4,324 to 4,367 vs 318 to 319 ns |
| Cloudflare Workers through workers-rs 0.8.7 and wasm-bindgen 0.2.129 (`worker-build --release` with blaze as its cargo build), nothing of rustc's | under `wrangler dev` 4.148.0 the hello and a SHA-256/HMAC worker (sha2, hmac) answer as rustc's builds; 1,218,107 vs 145,034 B and 1,304,087 vs 159,091 B; p50 1.44 to 1.47 vs 1.36 to 1.37 ms, and 3.04 to 3.14 vs 1.60 to 1.64 ms hashing 10 KiB |
| the same workers with `blaze build` as worker-build's cargo build, and the `no_std` simd128 guest of `allblaze_wasm.py` with `blaze build --release --target wasm32-unknown-unknown`, at main 2acba39f76 | std unit blaze's (18 crates for the workers, 3 for the guest); the workers answer as rustc's builds, 983,740 and 1,060,993 B, 88.8 and 81.3 s from a store that starts empty, p50 1.49 to 1.54 vs 1.41 to 1.44 ms and 3.07 to 3.16 vs 1.63 to 1.72 ms; the guest answers 7 of 7 calls as rustc's, 378,591 B vs 4,904 B, 4.4 s |

The corpus rows are of the comparison with rustc deleted on 2026-10-30. The train runs stress
and `allblaze_wasm.py` on every push to main
(`harness/wasm32.py`, [harness.md](harness.md)).

Limits: no unwinding is emitted (the wasm targets are `panic=abort`), threads are not
supported, `simd_fma` is refused (wasm has no fused multiply-add; LLVM calls libm's for
each lane), and the std unit refuses 84 functions, all `f16` or `f128`, which no program
here reaches. The modules are 8 to 50 times the size of rustc's and run 1.1 to 14 times as
long: blaze has no LTO or inlining across crates and keeps SIMD values in memory. In
workers-rs, five js_sys functions (`MaybeIterator`, `Global`) are refused for type errors,
and no module reaches them. `blaze build --target wasm32-*` of a package with proc
macros needs blaze's proc-macro server beside blaze (`blaze-proc-macro-srv`, which releases
ship). With a store
that starts empty, the workspace load compiles every build script at once, a blaze process
each of up to a few GB: on a 16 GB machine `CARGO_BUILD_JOBS=2` kept it at 9.6 GB, where the
default was killed at 14 GB. wasm-bindgen runs again after every edit, 0.27 to 0.36 s on a hello world
([reports/WASM32.md](reports/WASM32.md)). There is no LLVM backend for wasm.

Running the compiler itself in a browser is a separate experiment in `crates/blaze-wasm`.

## Still refused on every platform

- `sym` in an `asm!` block that is not naked, a `sym` that names a generic function, and
  one that names a thread-local.
- A naked function whose assembly defines a symbol of its own or has data in another
  section.
- Some variadic definitions. On x86-64 System V: a named parameter that is on the stack or
  is not an integer or float of at most eight bytes. On Windows: a named parameter wider
  than eight bytes or passed as an aggregate, and a definition with no named parameter. On
  AAPCS64 and Apple: a named aggregate.
- An argument in memory aligned to more than 16 bytes, and a vector in a C ABI.
