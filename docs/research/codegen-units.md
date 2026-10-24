# Codegen units for whole-program builds

2026-10-24, issue #176. Claims are marked **read** (measured here, with the command, or in a
source we read), **reported** (someone says so, unchecked) or **unverified** (our inference).
Hosts: c8a.24xlarge (x86-64) and c8g.24xlarge (AArch64), every build pinned to 16 cores with
`taskset -c 0-15`, `harness/runbench.py` against `cargo build --release` of the same tree.

## The question

A whole-program build compiles each function in an LLVM module of its own, with the bodies it
imports linked in ([`imports.rs`](../../crates/hir-ty-codegen/imports.rs)). ripgrep, helix and
nushell took about twice as long as cargo's release build (12.4 against 6.0 s, 70.6 against
33.5 s, 190 against 79 s, as the issue had them). Does compiling many functions to a module, as rustc compiles a
crate's codegen units, bring them to cargo's time with no slower a program?

## What was built

`RA_JIT_UNITS=1` ([`units.rs`](../../crates/hir-ty-codegen/units.rs)) compiles a whole-program
build in units; without it nothing changes.

- A function's IR is built without LLVM's part (`Detach::Unit`), and its imports are chosen
  as its compile would choose them. What it calls that it does not import, what those imports
  call that is not imported in turn, and every function it takes the address of, are found
  next: the functions are found in waves, from the roots, before any is compiled.
- The functions found are cut into units, in the order they were found in, of about 20,000
  instructions of Cranelift IR (finer when the build is too small to give each core four), and
  compiled on every core as they are cut, the largest first.
- A unit is one module: its functions, each in a comdat of its own so the object's section of
  each is that function's code, and the union of their imports linked in as `linkonce_odr`.
  An import LLVM left a call of after the passes is kept: its code is the code of the function
  it is of, unless a unit compiles that function as one of its own. So what a unit's code
  calls is known before LLVM runs, and the functions are found in one round.
- The code of each function is read out of the unit's object with what it refers to, and goes
  in the program's object as the code of a compile of that function alone does: one code path
  for codegen, a module of one function or of many. Builds that take edits compile one
  function to a module, as the splices need.

Linking the imports in one at a time took time of the square of their number, since LLVM's
linker goes over the whole module it links into: lowering ripgrep's units took 23.7 s summed
over the cores, where its functions compiled alone took 12.3 s (read, `RA_JIT_STATS`). The
imports are now linked to each other two at a time and into the module once, which took it to
8.1 s (read). Compiles of one function link theirs the same way.

## What it measured

`harness/runbench.py` through `base.sh` (three runs of each program, the median; one build
each), 2026-10-24 (read). "main" is the binary of `main` before this lane, measured the same
morning on the same hosts; "one function" and "units" are this lane's binary without and with
`RA_JIT_UNITS=1`. Build time in seconds, then the program's run time against cargo's release
build:

| x86-64 (c8a.24xlarge) | cargo release | main | one function | units |
|---|---|---|---|---|
| ripgrep | 6.0 | 12.6, 1.19x | 12.2, 1.15x | 12.5, 1.16x |
| helix | 33.4 | 75.4, 1.23x | 69.7, 1.21x | 71.1, 1.28x |
| nushell | 79.5 | 230.0, 1.07x | 207.3, 1.07x | 214.6, 1.08x |
| bun | 135.3 | 131.6, 1.23x | 127.0, 1.22x | 125.0, 1.21x |

| AArch64 (c8g.24xlarge) | cargo release | main | one function | units |
|---|---|---|---|---|
| ripgrep | 8.4 | 15.6, 1.32x | 14.4, 1.33x | 15.0, 1.35x |
| helix | 43.0 | 96.4, 1.12x | 88.0, 1.15x | 87.7, 1.12x |
| nushell | 82.0 | 284.2, 1.03x | 248.9, 1.02x | 234.1, 1.03x |
| bun | 167.6 | 162.5, 1.17x | 154.1, 1.17x | 150.5, 1.16x |

- Neither reaches cargo's release build but bun, which did before (read).
- "main" to "one function" is mostly the hosts, not the change: LLVM's time summed over the
  cores is the same within 1% (helix 790 against 783 s, nushell 2,415 against 2,394 s, read
  `RA_JIT_STATS`), of which linking the imports two at a time takes 7 to 10% off lowering.
- Units against one function: within 3% either way on build time, but for nushell on AArch64
  (6% faster), and the programs run as fast or slower (helix on x86-64 1.28x against 1.21x).

`RA_JIT_STATS=1` on helix, x86-64, the same binary with and without units (read):

| helix | one function | units |
|---|---|---|
| functions compiled by LLVM | 40,643 | 50,607 (and 49,397 imports kept) |
| imported instructions | 17.8 M | 8.8 M |
| LLVM, summed over cores | 785 s (lower 87, optimize 377, emit 118) | 568 s (lower 62, optimize 322, emit 158) |
| exports built | 201,200 in 180 s | 224,578 in 192 s |

- Units import half as many instructions and LLVM's summed time is 28% less (read), but the
  build is no faster: the cores are busy about three quarters of the units' 53 s (read, the
  time summed over the cores against 16 times the wall time). They wait on the waves: a wave's
  functions are cut into units only once all of it is built.
- Finding the functions from IR compiles a quarter more of them than finding them from the code
  of each does (50.6 k against 40.6 k, read): what LLVM folds away is found all the same.
- A function some units import and keep is compiled in each of them: ripgrep's units kept
  5,231 imports, of which 1,414 were the only code of their function, 1,149 were compiled as
  functions of a unit as well, and 2,668 were kept by another unit too (read, counted at the
  merge).
- helix runs slower on x86-64: 1.28x of release against 1.21x (read), though not on AArch64
  (1.12x against 1.15x). Which part costs it is not known (unverified: the union of the members' imports under one
  budget, or the kept copies' code).
- ThinLTO's order, a unit's own functions through the pre-link passes before their imports are
  linked in and both through the post-link passes after, took ripgrep's optimization from
  45.2 to 42.3 s but ran helix at 1.35x (read), so the unit goes through the passes whole.

## Why cargo is still twice as fast

- rustc's LLVM takes about 25 s for ripgrep summed over the cores: cargo's build takes 45.6 s
  of user time, of which rustc's main threads (`-Ztime-passes`, `RUSTC_BOOTSTRAP=1`) are 20.2 s
  (read), and the rest is its LLVM threads (unverified). ra-jit's takes 100 s for code of the
  same size: ripgrep's `.text` is 2.87 MB in 8,718
  functions against rustc's 2.75 MB in 9,299 (read, `size -A`, `nm`).
- The difference is what ra-jit gives LLVM: 33,270 exports, each built and put through
  ThinLTO's pre-link passes at `-O3` (27 s), 139 k bodies imported into the units (1.2 M
  instructions), each simplified again where it is imported (read). rustc copies only
  `#[inline]` and small functions into each codegen unit and ThinLTO imports within a crate.
  Importing less runs slower: bun built in 93.6 s and ran 1.311x with half the room of today's
  118.0 s and 1.244x (read, `imports.rs`, 2026-10-23).
- So the programs need the inlining, and what makes them need more of it than rustc's is the
  IR ra-jit gives LLVM (unverified: [mir-gap.md](mir-gap.md) has the passes rustc runs on MIR
  that ra-jit does not, and the lowering to LLVM loses what Cranelift's IR does not say, such
  as `noalias` and value ranges). Grouping functions into modules does not change how much
  there is to optimize.

## What would

- Exports from units: a unit's functions after the pre-link passes are what the others import,
  as in ThinLTO, which would save building 200 k exports (180 s of helix's) once the waves
  allow a barrier (unverified).
- Units cut as soon as a share of a wave is built, not the wave whole, to keep the cores busy
  (unverified: 13 s of helix's 53 s).
- IR that needs less inlining to run as fast: the MIR passes and attributes above.
