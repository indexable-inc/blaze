---
status: plan
summary: blaze's own linker: one patch engine with a lasting layout, writing to a running process or an executable, replacing cc, lld, ld64 and link.exe.
date: 2026-10-28
---
# blaze's linker: one patch engine, two outputs

blaze stops linking with `cc`, `lld`, `ld64` and `link.exe`. A program's executable is made by
blaze's own engine. The engine keeps a lasting layout of the program: where each function,
static and table is. It writes the functions that changed and fixes the references to them.
It writes to one of two places:

- **a running process**: what `blaze-splice` and `blaze-live` do today;
- **an image file**: the executable on disk, patched in place.

A cold build uses the same engine on an empty image. There is no separate "full link" path.
When the engine can't make an image, blaze stops with an error that names what failed and
where. It never goes back to `cc`.

Andrew decided this on 2026-10-28: "build blaze around patching, not linking". He added: "no
fallback, even during the migration." When the engine takes over a target, the same change
deletes that target's `cc`/lld path. Nothing keeps that path behind a flag or as a backup.

This document covers:

- Andrew's four questions, answered;
- what exists elsewhere, briefly, as background;
- why one engine can serve both outputs, and what differs between them;
- the design, piece by piece, including inlining and tiered code;
- where base images come from in CI;
- how the engine is proven right;
- the stages, starting with the first pull request.

Evidence is marked as elsewhere in the docs. **read** means measured or read in code by us.
**reported** means someone else's number, with a link. **unverified** means a target or an
inference. This document has no new measurements. The spike that was to compare lld, mold and
wild on tokio's release test binaries, and std as a dylib, could not run: `ix new` found no
x86_64 node that would take a VM (`vm.placement.capacity_exhausted`, 2026-10-28), and three
arm64 VMs went to `FAILED` before they booted. The numbers below come from the repository's
own reports and from published benchmarks.

## The questions

**What is the fastest linker we can have?** One that does no work for what didn't change. Every
linker that exists today repeats the whole link on every run. Today the fastest of them link
rust-analyzer in about 0.1 s (wild) to 0.25 s (mold) (**reported**, wild's benchmarks). blaze
already knows, per function, what changed: the code identities and marks of
[cache.md](cache.md), and the stable names of `hir-ty-codegen/stable.rs`. An engine that keeps
the last image's layout and rewrites only the changed bytes does work in proportion to the
edit, not the program. A tiny body edit already reaches a running program in 1-2 ms through
the splice ([roadmap.md](roadmap.md#the-floor-stage-by-stage), target). An image file can be
patched at the same cost, plus the write.

**Should it be dylibs?** No, not as the default for tests or dev builds. Under the
superset-of-rust rule, the program has to behave as rustc's static build behaves. One dylib per
dependency changes symbol interposition, TLS models, startup, constructor order and the
identity of generics ([below](#dylibs-not-as-a-default)). The engine gets dylibs' saving
another way: one std and dependency block, laid out once and shared by every test binary, in a
program that is still static ([shared blocks](#shared-blocks-one-std-and-deps-image-for-many-test-binaries)).
Dylibs stay where rustc makes them: the `dylib` and `cdylib` crate types, `-C prefer-dynamic`
when the user asks for it, and proc macros.

**Should we write our own?** Yes. No existing linker has an incremental mode a Rust toolchain
can use:

- wild's incremental linking is a design from 2024 that was never built;
- MSVC's is closed source and Windows only;
- Zig's is tied to Zig's own code generator.

Each of these also falls back to a full link when it can't patch, which blaze's rules forbid.
blaze also has inputs no linker takes: functions in the store, with their relocations, unwind
information and lines, which never need to become object files. The engine reuses code that is
already blaze's (`blaze-splice` writes Mach-O, PE, wasm and ELF patches). Parts of `libwild`
can be forked where its ELF knowledge is worth having ([where it lives](#where-it-lives-and-what-it-reuses)).

**What is best long term?** The engine in this document, for every target blaze builds. The link
becomes a write of changed functions into a layout that lasts. That layout is kept in the store,
like the functions it holds, so CI and every machine start from a base and not from nothing.

## What exists

As of 2026-10-28 (**reported**; the links have the details):

| Linker | Formats | Speed | Incremental | What it would give blaze |
|---|---|---|---|---|
| [wild](https://github.com/wild-linker/wild) 0.11 (Rust, MIT/Apache) | ELF (x86-64, AArch64, RISC-V, LoongArch); wasm without debug info since 0.11; Mach-O not on by default; no PE ([#2320](https://github.com/wild-linker/wild/issues/2320)) | rust-analyzer ~0.10 s against mold ~0.25 s and lld ~0.83 s; zed debug 0.6 / 1.3 / 4.8 s ([Ryzen 9955HX](https://github.com/wild-linker/wild/blob/main/benchmarks/ryzen-9955hx.md)). mold's own benchmark disagrees; [Lattimore](https://davidlattimore.github.io/posts/2026/09/18/benchmarking-wild-vs-mold.html) traces the gap to tmpfs and an existing output file | Planned ([design, 2024-11](https://davidlattimore.github.io/posts/2024/11/19/designing-wilds-incremental-linking.html), [#184](https://github.com/wild-linker/wild/issues/184) idle since 2024-11). Falls back to a full link on any failure | The fastest full ELF link, and `libwild` / `linker-diff` to fork and to check against. Not an incremental engine |
| [mold](https://github.com/rui314/mold) 3.0 (now Rust, MIT) | ELF, many architectures; Mach-O work in a crate that has not been released; no PE, no wasm | "4.9x faster than lld" at the median, by its own benchmark | Ueyama is "not against" it ([2.42.1](https://github.com/rui314/mold/releases/tag/v2.42.1)); nothing built | A fast full ELF link |
| lld (ELF, COFF, Mach-O, wasm) | all four | rustc's default on x86_64 Linux since 1.90; its Mach-O port is 2.4-6.8x slower than Apple's new linker on Chromium ([crbug](https://issuetracker.google.com/issues/502338406)) | none | What blaze uses today on ELF and wasm |
| Apple ld-prime (Xcode 15+) | Mach-O | "up to 5x" ld64 | none | What blaze uses on macOS today. Closed source |
| [Zig's linker](https://ziglang.org/download/0.17.0/release-notes.html) | ELF incremental with DWARF since 0.17; Mach-O incremental is future work; COFF and wasm not incremental | 194 ms to 65 ms per update on the Zig compiler (0.16) | Yes. The compiler writes relocated symbols straight into the output; calls go through an offset table; hot swap goes through ptrace or `mach_vm_write` | The closest design to this one, as a reference. The code is Zig, coupled to Zig's code generator |
| MSVC `link.exe /INCREMENTAL` | PE | | Padding plus an incremental link table of thunks, and a `.ilk` file. Falls back to a full link, with only a warning, on `/OPT:REF`, an added object, a changed option | Shows what padding and thunks cost. Closed |
| [subsecond](https://github.com/DioxusLabs/dioxus/blob/main/packages/cli/src/build/patch.rs) (Dioxus) | ELF, Mach-O, PE, wasm | | Hot patches only: a patch dylib linked against the running binary's addresses, reached through a jump table | A reference for ASLR and wasm patching |
| Makepad | | | No Rust code is patched: `--hot` reloads its UI DSL only ([book](https://makepad.rs/guide/start/makepad-framework-architecture)). "Relax", which [roadmap.md](roadmap.md) names, could not be found published | Nothing to reuse |
| gold `--incremental` | ELF | | Deprecated in binutils 2.44 | Nothing |

Three conclusions:

1. Speed differences between full linkers are about 2-8x. Patching instead of linking is a
   different order: the work follows the edit.
2. Every incremental linker that exists falls back to a full link. blaze can't take that
   behaviour as it is.
3. blaze's inputs are already per function and content-addressed. That is the part every one of
   these linkers has to rediscover from object files and timestamps. Wild's design spends most of
   its words on diffing inputs; blaze has the diff before codegen ends.

## One engine, two outputs

Patching a process and linking a file look like different jobs. They share the hard parts:

| Shared by both outputs | What it is |
|---|---|
| The layout | Where every function, static, constant, vtable and table is, keyed by code identity (the stable names of `stable.rs`) |
| Resolution | Which definition a name refers to: the program's own, an archive member's, a shared library's import |
| Relocations | Applying each relocation kind of each architecture, and the stubs, GOT slots and veneers a reach limit needs |
| Unwind | `.eh_frame` and its search table, Mach-O `__unwind_info`, PE `.pdata`/`.xdata` |
| Debug info | DWARF units and their lines (later, PDB) |
| Thread-locals | The TLS block's layout and the offsets code uses into it |
| Statics that last | `lasting.rs`'s rule: a static's place survives edits, and so does its value in a process |

`blaze-splice` already does every row of that table, for the case where the base was linked by
someone else. It reads the linker's image, lays the patch out behind it, resolves against the
image's symbols, rebuilds the unwind search table, rewrites the debug sections behind the image,
re-encodes Mach-O fixups and keeps the signature's page hashes
(`crates/blaze-splice/src/{lib,macho,pe,wasm,debug,lasting}.rs`, **read**). The engine is that
code, given a layout of its own instead of one recovered from another linker's output.

What differs between the outputs:

| | Running process | Image file |
|---|---|---|
| Old code | Must stay: a thread may be running in it, and a return address on a stack may point into it. A replaced function gets a jump at its entry, or its slot in the patch table changes ([roadmap §6](roadmap.md#6-own-the-binary-patch-in-place)) | Can go. Nothing runs the file while it is written. A replaced function's callers are fixed directly through the reverse reference index, so a patched image runs as fast as a cold one. Its old bytes become free room in the layout |
| Moving things | Nothing that exists may move: a function pointer or vtable stored in the heap still points there | Anything may move, and every reference to it is fixed |
| Statics | The value lives in memory; a static with a new layout is new (`MOVED_STATIC`), and a lasting one keeps its value | The initial value is written. A static whose layout changed is rewritten in place, or moved with its references fixed |
| Thread-locals | The TLS block of a running thread can't grow. A patch that grows it can't be applied to the process: the reply says the program must restart, and the image is still patched. This is the live protocol's existing "restart needed", stated as a result. It is not a fallback | The block is laid out again |
| Addresses | The process's own, after the ASLR slide | Unrelocated image addresses, plus the loader's relocations (`R_*_RELATIVE`, Mach-O rebases, PE base relocations) |
| Unwind | Frames registered at run time, and the search table written where the unwinder reads it | `.eh_frame_hdr` written sorted; `__unwind_info` generated; `.pdata` sorted |
| Headers | None | Program headers, dynamic section, symbol tables, build ID, and on macOS the code signature |
| W^X | Pages are made writable and executable in turn, `MAP_JIT` on hardened macOS, and the instruction cache is flushed | None. The file is written, and on macOS written to a new file that then replaces the old one, since the kernel caches a signature by vnode (`macho.rs` already does this) |
| Reproducible | No | A cold image is a function of its inputs. A patched one is a function of its base and its edit ([below](#base-images-in-ci-the-store-and-not-the-runner)) |

Neither column needs a different engine. The image output has more freedom: it can fix callers,
move things and compact. The process output has more constraints: old code stays, nothing moves,
and the TLS block can't grow. Both are the same planner run with different rules. An edit
normally produces both, in one step: the process gets the patch, and the image on disk gets the
same functions, so the next process start runs the new code without a link.

A cold build is the image output from an empty layout: every function is new, so every function
is placed. It is the same code path, and there is no second implementation to keep in step.
This is also what makes the no-fallback rule easy to hold. Nothing exists to fall back to.

## The design

### Inputs: functions from the store, not object files

Today codegen stitches the store's entries (`hir-ty-codegen/cached.rs`: code, relocations by
name, unwind, lines) into one object per build. `link/split.rs` then cuts std's object into 17
archive members so lld can spread it over threads, and lld parses all of it back. Hello's link is
0.16 s, 0.12 s of it lld demoting std's 140,000 sections on one thread before the split
(**read**, `split.rs`).

The engine takes the entries themselves: a function's bytes, its relocations against stable
names, its unwind rows and its line rows. Nothing is written as an object and read back.
`split.rs` goes away when ELF is taken over.

Some inputs are files and stay files. The engine reads them with `object` and adds their
sections to the layout like any function:

- the static archives and objects of `#[link(kind = "static")]` and `cargo:rustc-link-lib`;
- what the `cc` crate builds in build scripts;
- the C runtime's start files.

Shared libraries are read only for the names they define: `.so` dynamic symbol tables with
their versions, Mach-O `.tbd` text stubs and dylibs, PE import libraries and `raw-dylib`
(written directly, with no `lib.exe`).

An input the engine can't handle stops the build with an error naming it:

    blaze: libfoo.a(bar.o): relocation R_X86_64_GOTPC32_TLSDESC at .text+0x1c4 is not supported by blaze's linker (docs/linker.md)

The C toolchain stays a source of files, not a linker. Its start files, `libgcc_s`/`libgcc.a`,
and libc and its linker scripts (`libc.so` on glibc is a text `GROUP(...)` script) are found
from its search directories, once per target, and kept with the target's settings. The engine
parses GNU linker scripts of the kind system libraries ship: `GROUP`, `AS_NEEDED`, `INPUT`. A
full linker-script language is not taken. A user's `-C link-arg=-T...` is an error that names
it, until it is.

### The lasting layout

The layout is a table from code identity to (section, offset, size, room), plus:

- **Room.** A function is placed with room behind it: its size rounded up to a size class (say
  1/8 more, at least 16 bytes; **unverified**, to be tuned on edit traces). A grown function stays
  where it is while it fits. Past that it moves, and its references are fixed. Each section has
  room at its end, as the splice's `ROOM` does today. No size is a reason to stop
  ([architecture.md](architecture.md#no-size-is-a-reason-to-link-in-full)): what outgrows its
  room moves behind everything else.
- **The reverse reference index.** For every target, the places that refer to it: call sites,
  address loads, vtable entries, data relocations, unwind and debug references. A moved or
  removed target is fixed or freed through it. This is wild's planned "reverse index from
  symbols to relocations". blaze builds it from the store's per-function relocations, which it
  already has.
- **Free room.** Freed and outgrown places are put back in a free list per section, so an image
  that is patched a thousand times does not only grow.
- **Order.** A cold layout places functions in a deterministic order: crate, then the order
  codegen found them in, as units are cut today (`units.rs`). This keeps a function near what it
  calls and makes a cold image the same on every machine.
- **Garbage collection.** What the program can't reach is not placed. That is lld's
  `--gc-sections`, and blaze already depends on it: the link proves a stand-in unreachable by
  leaving it out (`link.rs`, `stand_ins`). The engine computes reachability itself from the
  roots: `main`, the test harness's list, `#[used]`, `#[no_mangle]` exports of a `cdylib`,
  `.init_array`. A stand-in that is reachable is the same error as today, and found earlier.

The layout is saved as one deterministic CBOR record next to the image, and in the store,
keyed by the image's hash.

### Shared blocks: one std and deps image for many test binaries

A package's test binaries link the same std and mostly the same dependencies. Today each one
links all of it again. The engine lays out a **block** once: a set of functions and data closed
under what they refer to, placed relative to its own base. It is kept in the store under the
hash of its members' code identities and the target. A test binary is then:

1. the block, copied into the image: a reflink where the filesystem has them (APFS, btrfs, XFS),
   so twenty binaries share one copy on disk;
2. the test's own functions, laid out after it;
3. the references between the two, fixed in the test's part. References from the block to code
   that differs per binary go through slots that the test's part fills. Those are `main`, the
   harness's list, `#[global_allocator]`, the panic handler, and generic instances the block
   needs but the test instantiates. The slots are a table inside the static image: no loader,
   no interposition.

The program stays a static executable: one TLS block in the local-exec model, no exported
symbols, rustc's `.init_array` order. It behaves as rustc's does, which dylibs would not. What
differs from rustc's output is that a block holds functions this binary can't reach. That is
size, not behaviour, and the stand-in check reads reachability from the layout and not from what
was placed. A release artifact that ships (`blaze build --release` of a binary) is laid out
without shared blocks, so it holds exactly what it reaches.

Linking one test binary becomes writing its own code and fixing its references to the block.
How much of the 1-3 minutes per release test link this removes depends on why those links take
so long, which another lane is finding out. This document makes no claim about that.

### Dylibs: not as a default

What would differ between a static test binary built by rustc and the same tests against one
dylib per dependency:

- **Interposition.** Default-visibility symbols in a `.so` can be interposed and bind through the
  GOT. Code rustc's static link called directly then resolves at load time. An `LD_PRELOAD`
  shim, or two dylibs defining one `#[no_mangle]` name, picks another definition than the static
  link does. Wild measured 291k `GLOB_DAT` relocations into `libbevy_dylib.so` itself
  ([**reported**](https://davidlattimore.github.io/posts/2024/08/27/rust-dylib-rabbit-holes.html)).
- **TLS.** A `thread_local!` in a dylib uses the initial-exec or general-dynamic model instead of
  local-exec. It costs `__tls_get_addr` calls, and its `dlopen` and static-TLS-surplus behaviour
  differs.
- **Startup.** Every dylib is loaded and relocated at start. Wild's measurement: about 150 ms of
  load for bevy's dylib, 5 ms with protected visibility (**reported**). A test suite of many
  short processes pays that per process.
- **Constructors and `#[used]`.** `.init_array` runs per library, in dependency order, not in the
  one order of the static link. A `#[used]` static in a dylib nobody references is still loaded,
  and its constructor runs.
- **Generics and statics.** A crate must appear once among everything linked, or its statics and
  TLS are duplicated ([rust#90539](https://github.com/rust-lang/rust/issues/90539)). The `dylib`
  crate type does not export shared generics, so instances are duplicated across libraries, and
  `TypeId`s and function-pointer equality can differ.
- **Panics and unwinding.** Unwinding crosses library boundaries through each library's
  `.eh_frame_hdr` (`dl_iterate_phdr`), and `panic = "abort"` crates linked with unwind ones
  behave per library.

Each of these is a behaviour difference, so under superset-of-rust a dylib-per-dependency
default is not allowed. Shared blocks buy the same saving without any of them.

### Formats

**ELF (Linux; x86-64, then AArch64).** The engine writes everything `link.rs` asks lld for today:
PIE, `--as-needed`, `--eh-frame-hdr`, `-z noexecstack`, `-z relro`, `-z now` (no lazy PLT),
`--build-id` (the live protocol finds a program by it, so a patched image gets a new one),
`--gc-sections` as above, `-Bsymbolic` and a version script for shared outputs. It also needs:

- dynamic imports with symbol versions: glibc gives an unversioned import the oldest version,
  which is the `realpath` trap `link.rs` already notes;
- `DT_NEEDED`, `PT_INTERP`, `PT_GNU_STACK`, `PT_GNU_RELRO`, `PT_TLS`;
- copy relocations and IFUNCs, when an input has them;
- TLS relaxations for C inputs.

Each relocation kind the engine accepts has a differential case. Any other kind is an error.

**Mach-O (macOS, arm64).**

- `LC_DYLD_INFO` opcodes. The splice reads fixups in tables today (`-no_fixup_chains`).
  Chained fixups can come later, once the process output writes them.
- `__unwind_info` written by blaze, `__eh_frame` for what it can't encode.
- Thread-local variable descriptors.
- Two-level namespace imports from `.tbd` stubs.
- `-dead_strip`, `-exported_symbols_list`, `-headerpad`.
- **The code signature.** arm64 macOS kills an unsigned or mis-hashed page. The engine writes an
  ad-hoc `CodeDirectory` with SHA-256 page hashes (the splice has `sha256.rs` and rewrites page
  hashes today), and the entitlements blob that `live.md` has `codesign --entitlements` add. So
  no `codesign` runs either.

Patching re-hashes only the changed pages.

**wasm.** The splice already writes a module again with new functions (`wasm.rs`). The engine
adds what `wasm-ld` does for blaze today:

- segment merging by name (the stand-in check reads segment names);
- the function table and indirect call indexes;
- `--export`, `--stack-first`, `-z stack-size`, `--no-entry`;
- wasi-libc's `crt1-command.o` and `libc.a` from the toolchain's self-contained directory;
- `--strip-debug`.

**PE/COFF (Windows x86-64).** Import tables from import libraries and `raw-dylib`, written
directly without `lib.exe`. Also `.pdata`/`.xdata`, base relocations, `/DEFAULTLIB` from C
inputs, `/WHOLEARCHIVE`, `/OPT:REF`, and `/EXPORT` for DLLs. Debug info means a PDB, which is
its own large piece: until the engine writes one, a debug build on Windows stays on the stage
before the takeover. An image without debug info when it was asked for would be a quiet
difference.

### Inlining, LTO and the JIT

A patch is correct only if it replaces every placed function whose bytes depend on what
changed. When `g` is inlined into `f`, an edit to `g` changes `f`'s code too, and the patch has
to carry `f`. So the identity the engine compares has to cover what was inlined into a function,
and not only its own source.

What `hir-ty-codegen` does today (**read**, 2026-10-28):

- **The identity the patch planner compares is the code itself.** A function's serial is
  `print_of` its compiled code (`root_serial`, `instance_serial` in `hir-ty-codegen/mod.rs`): a
  128-bit stable hash of the machine code, relocations and requests that `compiled_instance`
  produced. Inlined bodies come from `inlinable`, a salsa query marked `no_eq`. When `g`'s
  inlinable body is computed again, every `compiled_instance` that read it is computed again, and
  `f`'s print changes exactly when its bytes do. The planner (`patch.rs`, `Mark`) puts in the
  patch every function whose mark differs from the base. **Sound**: what was inlined is in the
  bytes, and the bytes are what is hashed. It is also precise: a callee that was not inlined
  changes only its own print.
- **The LLVM backend imports callees across crates** as `available_externally` bodies, ThinLTO
  style (`imports.rs`). The imported IR is read through the same queries, so the same argument
  holds: the caller's emitted code, and with it its print, follows what was imported.
- **Tiered code** (`tier.rs`) keeps, with each level-2 or level-3 body, the print of the level-1
  code of the function and of every function built into it. An edit to any of them demotes the
  function to level 1 in that same build (`Tiers::take_demoted`). **Sound**, by the same rule.
- **The disk store's key is not the code print.** It is the stable name: path, generic
  arguments, crate ([cache.md](cache.md#what-is-stored-and-under-what-key)). That is sound only
  because naming is coarse. A crate of the checkout is named by a hash of its source, and an edit
  renames every crate of the checkout that depends on it, so an entry can't hold a stale inlined
  body from the checkout. A dependency is named by its version and cargo's metadata, which don't
  change without its source changing. A generic dependency function instantiated with a checkout
  type carries that type's crate name. This is correct and over-invalidates. It is not a
  per-function identity of what was inlined.

What the engine needs, then:

- **It patches by print, never by name.** For each placed function, the layout records the print
  of the bytes placed. A function is rewritten when its print changes. Inlining, imports and
  tiering are then covered without the engine knowing about them.
- **Inline sets travel with the print.** The store keeps, per entry, the prints of what was
  inlined or imported into it (tier.rs already records them for tiered code). That lets a shared
  block's entry be checked, not trusted: an entry whose inline set names a print the current
  build has another of is not used. The over-invalidation of today's key can then be loosened in
  a later change, with `harness/keys.py` extended to edit an inlined callee in another crate.
- **Cross-module LTO at link time is not the engine's job.** All optimisation happens in
  codegen, per function or per unit, before the engine sees code. The engine places bytes and
  fixes references. It does no code folding (ICF) and no LTO, which keeps "what changed" equal to
  "what codegen produced anew".

**Tiered compilation in live mode** (a later stage, on top of what `tier.rs` does today). A
program running under `blaze-live` starts on level-1 Cranelift code. The daemon samples it and
recompiles hot functions at LLVM `-O3`, then with PGO from counters. It patches them into the
**process output only**, through the same engine with the process rules. Tiered code never goes
into an image file. It depends on a profile, so it can't be keyed or reproduced, and the image on
disk keeps the level-1 code of the last edit. Later work for that stage: a patch table slot per
tiered function, so that promotion and demotion are one store each, and on-stack replacement for
hot loops that never return. Neither is designed here.

**Release and shipped images stay deterministic and match rustc.** A release build, and any
image published as a base for others, is a cold layout from an empty image, compiled at the
profile's optimisation level by codegen. It holds no tiered code, no patched history and no
shared block. The same inputs give the same bytes on every machine. Its behaviour is held to
rustc's build of the same program by the differential harnesses
([proving it right](#proving-it-right)). Only dev and test images are patched in place, and only
running processes get tiered code.

### Debug info

The store holds each function's line rows and variable places (`FunctionDebug`). The engine
writes DWARF units per crate from them. The sections get room behind them, so that a patch
rewrites only the units it touched, as `splice/debug.rs` does today. Debug info is never a
reason to stop a patch ([architecture.md](architecture.md#no-size-is-a-reason-to-link-in-full),
rule 3). It is checked by the differential harness: every address maps to the same file and line
as in the lld-linked program of the same functions.

### Where it lives and what it reuses

A new crate, `crates/blaze-link`, outside the front end (rule semantics-in-front-end: codegen and
linking stay out of `hir-*`). It holds the layout, the planner, resolution, relocations per
architecture, and one writer per format.

- `blaze-splice` becomes the process output on top of it. Its format code (`macho.rs`, `pe.rs`,
  `wasm.rs`, `debug.rs`, `lasting.rs`, `sha256.rs`) moves into `blaze-link` as the engine takes
  each format. What stays in `blaze-splice` is writing into a process.
- `blaze-engine/src/link.rs` keeps what it knows that is not `cc`'s: the native libraries in
  rustc's order (`native`, `asked_by_script`, `lib_of`), `raw-dylib`, the wasm start files. It
  passes them to the engine as inputs, not argv. Its `cc` lines are deleted target by target.
- `object` reads every input format and writes low-level ELF and PE. `gimli` writes DWARF.
- `libwild` (Rust, MIT/Apache) is the most complete open ELF linker. Its handling of archive
  semantics, symbol versions, TLS relaxations, IFUNC and copy relocations can be forked into
  `blaze-link` where it is better than writing it again; its writer is not built for patching,
  so the layout and planner stay blaze's. `linker-diff`, wild's tool for comparing two linkers'
  outputs symbol by symbol, is the starting point of the differential check below.

## Base images in CI: the store, and not the runner

A patch needs a base. Where it comes from decides whether CI gets the saving.

- **The store is the source.** Shared blocks and layout records are content-addressed entries,
  like compiled functions. They go to the remote store ([cache.md](cache.md#the-remote)) with
  their hashes checked, as any entry is. A CI run for a commit fetches the blocks for its std and
  dependencies. These change only when the lockfile, the toolchain or blaze changes, so most runs
  hit. It also fetches the layout and image of each test binary from the commit's merge base on
  main, keyed by the binary's name and the base commit, and patches them.
- **A persistent runner is a cache of the store.** Its local disk store makes the fetch local.
  Correctness never depends on it: a fresh runner with an empty store does a cold image, which is
  the same engine and the same result for the same inputs.
- **Reproducible output.** A patched image depends on its base's history, so two runners that
  started from different bases can make different bytes that behave the same. Each image
  records its base's hash. What ships, a release artifact or anything published to the store as
  a base for others, is a cold image, a function of its inputs alone. A test binary in CI may be
  patched, since only its behaviour is read.
- **Trust.** A base from the remote is used only after its hash is checked, as for any store
  entry ([cache.md](cache.md#trust-and-signing)). A base whose layout record does not match its
  image is an error.

## Proving it right

rustc stays the reference ([contributing.md](contributing.md#rules-for-code)). The `cc`/lld link
is deleted from blaze when the engine takes a target over, but it stays in `harness/` as an
oracle. A harness may run a reference build, and blaze itself may not.

1. **Differential, per program.** Link the same functions with the engine, and with lld through
   the harness. Then compare:
   - the behaviour: the test results, stdout, the exit status;
   - the structure, symbol by symbol: every relocation resolves to the same target symbol and
     addend;
   - imports: the same `DT_NEEDED` and versioned imports;
   - TLS: the same block size and alignment;
   - `.init_array` order;
   - unwind: every function covered by an FDE with the same CFA rules;
   - debug info: every address maps to the same line.

   Byte equality is not the target, because layouts differ. Equality of what the loader, the
   unwinder and the debugger see is.
2. **Against rustc's build.** The program the reference build links behaves the same as
   blaze's: the existing differential harnesses (`allblaze.py`, `wholeprogram.py`, the bench
   apps), with the engine as the only linker.
3. **Real crates.** hello, then the `allblaze.py` programs, ripgrep, nushell, helix, bevy, and
   blaze linking blaze. Also crates with native libraries (`ring`, `openssl-sys`, `libz-sys`,
   `rusqlite` bundled) and with `raw-dylib` (`windows-link`).
4. **Patch against cold.** A property the engine has to hold: for any sequence of edits, the
   image patched edit by edit is equivalent, by check 1, to the cold image of the final program.
   `blaze-fuzz` generates the sequences: functions that grow past their room, removed and added
   statics, TLS that grows, vtables that change, edits that cancel each other. Every
   differential failure becomes a fixed seed.
5. **Inputs.** Fuzz the archive, object, `.so`, `.tbd` and linker-script readers on malformed
   bytes, since build scripts produce them: an error, never a panic or a write outside the
   output. Fuzz relocations: generated C objects with each relocation kind, linked by the engine
   and by lld, compared by check 1.
6. **Size.** `harness/bigbinary.py`'s rule applies: every fixed width gets a step there.

## Stages

Each stage lands through its own pull requests. A stage that takes over a target deletes that
target's `cc` path in the same change, along with the existing fallbacks around it. One of
those is the "kept as one object" path when `split.rs` fails, in `blaze-engine/src/lib.rs`,
which is a fallback today. Another is the splice's "the caller links in full", which becomes the
image output.

| # | Stage | What it wins (**unverified** until measured) |
|---|---|---|
| 1 | **First PR.** `crates/blaze-link`: a cold ELF image for x86-64 and AArch64 Linux from empty layout, enough for hello over blaze's std: the store's functions in, glibc dynamically with versions, crt files, PIE, `.eh_frame_hdr`, TLS, build ID, `RELATIVE` relocations. A harness (`harness/linker.py`) links hello both ways, runs both and compares as in check 1. No product path uses it yet, so nothing can fall back to `cc` | Nothing in CI yet. Hello linked by the engine in no more than lld's 0.16 s |
| 2 | ELF complete: archives and native libraries, linker scripts of system libraries, reachability and stand-ins, DWARF, shared outputs (`cdylib`, proc macros), copy relocations, IFUNC. Differential on the `allblaze.py` programs, ripgrep, nushell and the native-library crates | Faster than lld on ripgrep (0.25 s today, **read** in no-rustc.md), with std no longer split and parsed again |
| 3 | **ELF takeover.** `link.rs`'s ELF `cc` path and `split.rs` deleted in one change | ELF builds run no linker process |
| 4 | Lasting layout and image patching on ELF; the splice's ELF code moves onto the engine as the process output; an edit patches the process and the image together | A body edit's "refuse and link" from 28 ms to under 3 ([roadmap](roadmap.md#11-delivery-and-order), workstream 5); a test binary relinked after an edit in time proportional to the edit |
| 5 | Shared blocks and their store entries; base images from the remote in CI | The 20 release test binaries' link time, against what the other lane's diagnosis leaves of today's 11 minutes |
| 6 | Mach-O arm64, with signing and entitlements; takeover deletes the ld64 path | macOS builds with no `ld` or `codesign` |
| 7 | wasm; takeover deletes `rust-lld` for wasm, and with it blaze's last need for `rust-lld` ([self-contained.md](self-contained.md)) | The toolchain ships no lld |
| 8 | PE/COFF with PDB; takeover deletes `link.exe` and `lib.exe` | Windows builds without the MSVC linker |
| 9 | Tiered code through the engine's process output: a patch-table slot per tiered function, on-stack replacement for hot loops; store entries carry their inline sets and `harness/keys.py` edits an inlined callee across crates | Hot functions at LLVM speed in a live program without a rebuild; release images unchanged |

The first pull request is stage 1. It is small enough to review, it is checked against today's
output from the start, and it puts the crate where every later stage builds.
