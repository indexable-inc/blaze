---
status: record
summary: blaze on macOS aarch64: the first port.
date: 2026-10-20
---
# blaze on macOS aarch64

Branch `blaze-macos`, off `blaze-spike`. Everything below was run on one machine: an Apple M4
(virtual), macOS 26.5.2, `uname -m` says `arm64`, Apple ld 1267, nightly-2026-05-10. Where I
say "measured" I ran it there. Where I say "inferred" I did not.

The short version: blaze builds, the corpus matches rustc, every harness in the task list
passes, and the splice works on Mach-O arm64 for executables that use dyld info (which is
what rustc's default deployment target of macOS 11 produces). On ripgrep the splice takes a
link from 89 ms to 42 ms. I could not run anything on Linux, so "Linux is unchanged" is by
construction and not by test.

## What works (measured)

`cargo build -p blaze-engine --release` builds.

| harness | result here | Linux on `blaze-spike` |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases` | MATCH, 655/656 functions from blaze | 679/680 |
| `programs.py` | 22 of 22 | 22/22 |
| `stress.py` | 99 of 99, 0 restarts | 99/99 |
| `breakers.py` | 15 of 15, 1 open, 0 functions skipped | 15/15 |
| `unwinds.py` | 8 of 8 | 8/8 |
| `BLAZE_SPLICE=1 splices.py` | 71 of 71; 58 spliced, 13 full links | 71/71 |
| `BLAZE_SPLICE=1 worktrees.py` | 52 of 52; 39 links spliced, 5 full | 52/52 |
| `asmedits.py` | 10 of 10 | |
| `deps.py` | 10 of 10 | |
| `includes.py` | 9 of 9 | |

Also run: `BLAZE_SPLICE=1 programs.py` 22 of 22, and `splices.py` with `SPLICES_SOAK=100`,
100 of 100 with all 100 spliced and the last binary the same as cargo's.

Notes on those numbers:

- The corpus has 656 functions here and not 680 because the x86 SIMD and x86 `asm!` cases are
  behind `cfg(target_arch = "x86_64")`. The one function not from blaze is the same
  `type_id_hashes::elided` that is skipped on Linux.
- `difftest.py corpus` has to be run from `harness/`. From the repo root it looks for
  `./corpus` and fails. I did not change that.
- The 13 full links in `splices.py` are the cases meant to decline: 4 where the whole object
  was written again, 3 where the executable was touched, rewritten or stripped by someone
  else, 4 statics that grew, changed alignment or became writable, `_abs` not being in the
  executable, and `_memmove` having no address slot. Each ran the full link and printed the
  right output. On each spliced step the harness also runs `codesign --verify --strict` and
  `dyld_info` on the binary, and runs it with `DYLD_BIND_AT_LAUNCH`.
- The 5 full links in `worktrees.py` are first links of a checkout, where there is nothing
  to splice into yet. The spliced count moved between two runs (44, then 39); I did not look
  into why, the result was 52 of 52 both times.
- `breakers.py`: `clmul` is x86-only (PCLMULQDQ) and is an empty case here. `simd_cast` has a
  NEON version that I added, so it tests the same thing with different intrinsics.
- `asmedits.py`: the blocks are AArch64 instructions on this machine, so it tests the new
  AArch64 `asm!` wrapper. The x86 blocks are still there for x86 hosts, but I restructured
  the file to pick by architecture and could not run the x86 side.
- `deps.py` builds with `--offline`, so it needed the registry cache filled once
  (`cargo fetch` in a scratch project with the same dependencies).

## The LLVM backend (measured)

Everything above ran on Cranelift, the default. The LLVM backend also builds and runs here:
`brew install llvm@20`, then
`LLVM_SYS_201_PREFIX=/opt/homebrew/opt/llvm@20 cargo build -p blaze-engine --release --features llvm`
(LLVM 20.1.8). Homebrew has since dropped `llvm@20`; the supported LLVM is now the flake's,
the same 20.1.8 ([contributing.md](../contributing.md#llvm-for---features-llvm)). With `BLAZE_BACKEND=llvm`, at the default `BLAZE_OPT=2`:

| harness | result |
|---|---|
| `difftest.py corpus --bin runner --crate cases` | MATCH, 655/656 functions from blaze |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 15 of 15, 1 open |
| `unwinds.py` | 8 of 8 |
| `BLAZE_SPLICE=1 splices.py` | 71 of 71 (57 spliced, 14 full links) |
| `BLAZE_SPLICE=1 worktrees.py` | 52 of 52 |
| `asmedits.py` / `deps.py` / `includes.py` | 10 of 10 / 10 of 10 / 9 of 9 |

The daemon logs of programs, stress, unwinds, asmedits, includes, splices and worktrees have
a `backend: llvm compiled` line for every build and no fallback to Cranelift, so these runs
did go through LLVM.

`backends.py` says how much LLVM compiled itself. On the corpus, of 10770 functions, 2 are
left to Cranelift at both levels ("more than two return values"). On ripgrep 14.1.1 (the
`rg` crate, 14352 functions) none are, and the program prints the same as cargo's with
Cranelift, LLVM -O0 and LLVM -O2 (`rg --sort path -n -i 'splice|unwind\b' crates/blaze-engine/src`).

| ripgrep, `rg` crate | producing code, s (all threads) | code bytes | blaze wall, s |
|---|---:|---:|---:|
| cranelift | 2.1 | 5969204 | 2.8 |
| llvm -O0 | 7.7 | 6535252 | 3.7 |
| llvm -O2 | 22.4 | 1153564 | 6.5 |

`backends.py corpus/kernels kernels kernels --kernels`, best of 3, ms, same checksums in
every column:

| kernel | cargo debug | cranelift | llvm -O0 | llvm -O2 | cargo release |
|---|---:|---:|---:|---:|---:|
| sieve | 248 | 1448 | 1865 | 294 | 24 |
| collatz | 429 | 774 | 1004 | 274 | 93 |
| fnv | 232 | 1368 | 1824 | 365 | 59 |
| qsort | 755 | 1297 | 1653 | 437 | 160 |
| matmul | 435 | 3277 | 4378 | 374 | 16 |
| nbody | 681 | 2279 | 2842 | 409 | 19 |
| mandelbrot | 446 | 1230 | 1269 | 175 | 166 |
| words | 372 | 1328 | 1489 | 418 | 41 |

What changed for it: the target machine, triple and CPU (`apple-m1`, rustc's baseline here)
come from the host triple instead of being x86-64 Linux; inkwell's code generator feature is
chosen by the host architecture in `Cargo.toml`; and `llvm/emitted.rs` reads LLVM's Mach-O
object (underscored symbols, the arm64 page and page-offset, GOT, TLV and branch
relocations, `__compact_unwind`). On AArch64 every constant section the code refers to
becomes a data object of its own, because the code reaches it by page and only the linker
knows the page. The compact unwind entry says what the frame is once set up, not where the
prologue ends, so the rules are given from the first instruction on. That is right at
every call, which is where a panic unwinds, and wrong inside the prologue and epilogue,
which only matters to something that unwinds from an arbitrary instruction (a sampling
profiler, a debugger).

Not measured for LLVM: `BLAZE_OPT` other than 0 and 2 (0 only through `backends.py`), edit
latency against Cranelift, and the x86-64 side. The x86-64 path of the feature type-checks
(`cargo check -p hir-ty --features llvm --target x86_64-apple-darwin`) and the ELF code in
`emitted.rs` is the old code behind a format check, but it was not run.

## Edit-to-running, full link against splice (measured)

Small program: the `sedge` program of `splices.py`, 100 one-line edits in a row
(`SPLICES_SOAK=100`), each one synced over the socket. "Request" is from asking the daemon
to the daemon answering; it does not include starting the program.

| | link step, median | whole request, median |
|---|---|---|
| full link | 35.4 ms | 50 ms |
| splice | 9.6 ms | 23 ms |

Larger program: ripgrep 14.1.1, the `rg` crate (1,022 functions, 14,352 with instances),
`scaling.py` in watch mode, 5 kinds of edit and their reverts, 5 times each, 50 builds.
"Edit to binary" is the daemon's own figure and includes the file watcher noticing.

| | link step, median | edit to binary, median |
|---|---|---|
| full link | 89.3 ms | 104.8 ms |
| splice | 41.5 ms | 58.5 ms |

All 50 spliced, and the edit that prints printed. The daemon was at about 1.1 GB.

The splice is slower on ripgrep than on the small program and I have not profiled why. My
guess (inferred) is the copy of the executable and writing `__LINKEDIT` again, both of which
grow with the binary.

The binary does not grow from splice to splice. Each splice starts again from what the
linker wrote: the copy is cut back to where `__LINKEDIT` was and the whole patch is written
there, so the size depends on the patch and not on how many splices came before. Measured
on ripgrep (30,628,936 bytes linked): the same line of `main` edited 20 times gave
34,116,704 bytes after every one; adding and removing the line alternated between
34,116,704 and 34,000,912; a line added to a different function each time, all kept, went
from 34,017,472 to 34,133,216 over 9 edits, as the patch came to hold more functions. The
first splice costs about 3.4 MB here whatever the edit, nearly all of it the moved
`__eh_frame` (2,571,952 bytes) and the rebuilt `__unwind_info` (611,800). The "bytes" in
the daemon's `spliced:` line is what it wrote, the new segments and all of `__LINKEDIT`
(14.2 MB on ripgrep), not what the file gained.

## How the port is put together

Code generation. The target is the host triple instead of a fixed
`x86_64-unknown-linux-gnu`. The calling convention is `AppleAarch64` on Apple arm64 and
SystemV otherwise, and the TLS model follows the object format. Cranelift has no struct
arguments on arm64, so aggregates are classified by hand: homogeneous float aggregates and
small structs go in register units, larger ones by pointer, and Apple's variadic rule
(variadic arguments always on the stack) is handled. Unwind tables use the Mach-O relocation
for the personality, and the LSDA no longer goes into a section named `.gcc_except_table`,
which Mach-O cannot name. `asm!` has an AArch64 wrapper next to the x86 one.

Linking. Apple's linker has no `--allow-multiple-definition`. blaze's object has to win over
rustc's definitions of the same symbols, so on Mach-O the definitions in rustc's objects and
rlibs are made weak (`blaze-engine weaken`, used by the harnesses through `harness/linkline.py`),
and blaze's own objects are written the same way. Linux keeps the flag it had.

The splice (`crates/blaze-splice/src/macho.rs`). The loader wants `__LINKEDIT` last and the
signature at the end of it, so the patch goes where `__LINKEDIT` was, in two new segments
(`__BLAZE_TEXT` for code, constants, stubs and the unwind tables; `__BLAZE_DATA` for writable
data and address slots), and `__LINKEDIT` is written again behind them. That keeps W^X and
16 KB pages. Calls out of `b`/`bl` range go through a stub. The rebase opcodes are encoded
again to cover the new pointers. `__unwind_info` is rebuilt with the patch's functions and
`__eh_frame` is moved, with the old section headers renamed so the unwinder finds the new
ones. The ad hoc signature is rebuilt, keeping the hashes of pages that did not change. The
result is written to a copy that replaces the executable by rename.

It needs two more load commands than the linker leaves room for, so the link line has
`-Wl,-headerpad,0x200` (added in `linkline.py`). Without it the splice declines.

## What does not work, and why

1. Executables with chained fixups are declined. Measured: with
   `MACOSX_DEPLOYMENT_TARGET=13.0` the linker emits `LC_DYLD_CHAINED_FIXUPS`, and
   `splices.py` is 71 of 71 with every edit a full link ("an executable whose addresses are
   fixed up in chains"). The default target (11.0) gives dyld info and splices. Anyone who
   sets a newer deployment target gets no splice until this is done or the link line gets
   `-no_fixup_chains`.
2. Linux x86-64 was not run. There is no Linux machine in this session. `hir-ty` passes
   `cargo check --target x86_64-unknown-linux-gnu`; the `blaze-engine` crate could not be checked
   that way because `ring` needs a C cross compiler. The ELF splice path is the old code
   behind a format check, and the harness link line on Linux is the old one, but that is
   reading, not testing. Please run the list on Linux before trusting this branch there.
3. The LLVM backend's unwind rules on macOS are not exact inside prologues and epilogues
   (see the LLVM section), and it has no test of its own beyond the harnesses.
4. The splice writes a copy and renames it because I believe the kernel caches a file's
   signature once it has run (inferred; I never tried writing in place). If that is wrong,
   in-place writes would be faster on large binaries.
5. The AArch64 ABI code is tested only through the corpus and the programs. There is no
   targeted test for variadic calls with floats, large HFAs on the stack, or Apple's packed
   stack arguments beyond what those happen to cover.
6. The AArch64 `asm!` wrapper covers general and vector registers and the register classes
   `reg`, `vreg`, `vreg_low16`. Other classes and clobber-only ABIs are not handled, and the
   only test is `asmedits.py`.
7. `blaze-engine weaken` has only seen the objects and rlibs these harnesses produce.
8. `harness/linkbench.py` still passes `--allow-multiple-definition` to `ld -r` and was not
   ported or run. `bench.py`, `agents.py`, `dxedits.py`, `invalidation.py`, `uitest.py` and
   the other harnesses outside the task list use the new link line but were not run.
   `scaling.py` was ported (RSS through `ps`, a `SCALING_SPLICE` switch, and its log pattern
   now allows the `asked:` line the daemon prints) and run on ripgrep only.

## What is left, in order

1. Run the whole list on Linux x86-64.
2. Chained fixups in the splice, or `-no_fixup_chains` on the link line as a stopgap.
3. Find where the 40 ms goes on ripgrep and whether an in-place write is allowed.
4. See whether a splice can avoid copying all of `__eh_frame` (2.6 MB on ripgrep), for
   instance by giving the patch's functions compact entries only. Not tried.
5. Direct ABI tests for AArch64: variadics, HFAs, stack packing.
6. Exact unwind rows for LLVM code on Mach-O (ask LLVM for `__eh_frame` instead of
   reading the compact entry), and the LLVM backend on Linux again.
7. Port `linkbench.py` and run the remaining harnesses.
8. Make `difftest.py corpus` work from the repo root, as the task list writes it.
