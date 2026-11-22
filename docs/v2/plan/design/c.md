---
status: plan
summary: C in blaze without a build script. One pinned C toolchain shipped as records and run natively as blaze's own hermetic tool; a header is imported by a built-in macro whose expansion is a record of Rust declarations keyed by the headers it read; C sources are units compiled per file, lazily, to code records; existing crates' `cc` and `bindgen` calls are answered by the same machinery.
date: 2026-11-20
---
# C in blaze

The owner, 2026-11-20 18:56: "I think blaze should also ship a c compiler and do something
similar to the cImport ... what is the most idiomatic way pls do this". Decision D174.

**What Zig does, and what it changed.** Three separate things: the compiler ships a C compiler
(Clang) with libc headers for every target, so C compiles and cross-compiles with nothing
installed; a header becomes declarations automatically; and the build file declares steps
instead of running commands. The second was a language builtin (`@cImport`) until Zig 0.17
removed it: translation is now a build step from a separate package on their own C front end
(Aro), because a compiler that parses C at compile time is tied to a C compiler. blaze takes the
three ideas and that lesson.

**Why blaze needs it.** Six of tempo's dependencies run `bindgen` and a C compiler from a build
script (build-scripts.md: librocksdb-sys, reth-mdbx-sys, zstd-sys, c-kzg, libproc, aws-lc-sys);
one such script is 24 s of a first check. A build script is an opaque program: blaze cannot key
its work by what it read, cannot run only the part an edit touched, and cannot run it without
the machine's compiler. Rules 9 and 25 ask for the opposite.

## The four parts

**1. One C toolchain, pinned and shipped; the compiler runs natively, as blaze's own tool.**
toolchain-c.md already decides the toolchain: clang, lld, compiler-rt, libc++ and libc stubs
built from blaze's pinned LLVM, nothing from the machine. The compiler is NOT user code: it is a
pinned tool of blaze's, like the LLVM blaze already links, and the C it compiles is data, not a
program blaze runs. So it needs no sandbox of the kind rule 9 asks for build scripts and proc
macros; it needs HERMETICITY: it sees only the files blaze gives it (the package's sources and
declared include paths, and the shelf's headers), with no environment, no clock and no host path
in its output, and blaze learns every file it read. It runs in its own process (a crash in it
fails one task: rule 20), from the shelf, with its file access held to those roots by the
operating system and its reads reported back; the read list is checked against the roots, so a
read outside them is a refusal naming the file. No WebAssembly build of the compiler exists or
is wanted (first written here with one, and corrected the same day: the owner, 2026-11-20 19:01,
"web assembly clang? huh and why we need that"; a sandbox for a trusted tool was the rule
misapplied).

**2. A header is imported by a macro; the result is a record.**

```rust
blaze::c::import! {
    mod rocksdb;                       // the module the declarations land in
    header "rocksdb/c.h";
    include "vendor/rocksdb/include";
    define ROCKSDB_PLATFORM_POSIX;
}
```

It is a built-in macro, so it is an expansion like any other (the Names stage already keys an
expansion by its call text and the cells it read). Its expansion is ordinary Rust: `extern "C"`
functions, `#[repr(C)]` structs and unions with the target's layout, enums, and constants for
object-like macros; a function-like macro, an inline function body and a C++ construct are each
a finding naming it (rule 81), never a silent skip. The key is the call's text, the target, the
toolchain's digest and the digest of every header the front end read. Editing a header expands
that import again and nothing else; editing Rust expands nothing. It is a library feature over
records (rule 32), not a language builtin: the lesson of `@cImport`.

The front end behind the macro is a seam: `trait CFrontEnd` produces ONE declaration record
(blaze-c's model). Its first instantiation is the pinned clang of part 1. A second, blaze's own
declaration front end (preprocessor, declarations, ABI layout: no bodies), may replace it for
imports later; the two are held byte-equal on the same headers by a differential test, and
clang stays the compiler either way.

**3. C sources are units, compiled per file and only when reached.**

```toml
[package.metadata.blaze.c]
sources = ["vendor/rocksdb/**/*.cc"]
include = ["vendor/rocksdb/include"]
define  = { ROCKSDB_PLATFORM_POSIX = "1" }
std     = "c++20"
```

A translation unit is a task (a kind on the one driver, D157). Its key: its text, the headers it
read, its flags, the target, the toolchain. Its output: code records, one a function, as Rust
bodies make them, so there is no link step and a C function is called like any other. `check`
generates no code for C, as it generates none for Rust: to type-check Rust that calls C it needs
only the import's declarations. A workspace member's own C sources get the front end alone in
`check` (parsed and type-checked, no code made), a file a task under the same key, so an error
in a C file you edited is reported by `check` and an unedited file is never looked at again; a
dependency's C sources are not checked, as its Rust bodies are not. `run` compiles the units that define
a symbol the program reaches (an index from symbol to unit is a cheap record made from the
units' declarations), so a 300-file library a program uses a tenth of costs a tenth.

**4. Existing crates work unchanged.** A build script's spawn of `cc`, `c++`, `ar`, `cmake` or
`pkg-config` is already routed to the C mode (blaze-build-script probe.rs `C_TOOLS`): the C mode
turns each compile it is asked for into the unit task of part 3. A script that calls the
`bindgen` crate is given blaze's own crate of that name and API, which asks part 2's import and
writes the same `bindings.rs`. Neither runs the machine's compiler or libclang.

## C++ (decision D176)

The owner, 2026-11-20 19:02: "could we build the same thing as c import but for cpp? that would
be really awesome too". Yes, by the same seam, and blaze is placed to do it better than a
binding generator can, for three reasons it already has: it ships the C++ compiler (so it can ask
for a template to be instantiated and an inline function to be compiled, which no header reader
can do); it has no link step (so the code clang makes for the C++ side is code records beside
the Rust ones, and a thunk is not a build artifact anyone sees); and it owns the language (rule
55: a C++ type that Rust's move cannot express is given the type it needs, not worked around).

```rust
blaze::cpp::import! {
    mod rocksdb;
    header "rocksdb/db.h";
    include "vendor/rocksdb/include";
    std "c++20";
}
```

It is built in three steps, each useful alone, and the declaration model of step 1 of C is
written to hold all three (namespaces, overload sets, methods, references, and each type's
relocation fact), so C++ extends the record and does not fork it.
1. **What has a plain shape.** Namespaces as modules; free functions and overload sets (an
   overload is chosen by its parameter types; the symbol's mangled name comes from clang, never
   from blaze's own mangler); `enum class`; references as non-null pointers with the pointee's
   mutability; classes that are trivially copyable as `#[repr(C)]` structs with their methods.
2. **Classes with constructors and destructors.** A class clang says is trivially relocatable is
   an owned Rust value: its destructor is `Drop`, its copy constructor an explicit `clone`. A
   class that is NOT (it holds a pointer into itself: libstdc++'s `std::string` is one) is
   address-only: it lives where it was constructed and is reached by reference, which is an
   opt-in extension type of blaze's (linear.md, ownership.md), not a `Pin` convention. Which a
   class is, is a fact in the record, from clang, per target and standard library.
3. **What needs the compiler.** A template is instantiated on demand for concrete arguments
   (`cpp::std::vector<i32>`); an inline function or a method defined in a header is compiled by
   clang into a code record when Rust first calls it; a C++ exception is caught at the boundary
   by a generated thunk and returned as a `Result`, or the import says the function cannot
   throw; virtual calls go through the object's own table. Every thunk is C++ text blaze writes,
   clang compiles, and a record keeps: there is no bridge file, no build script and no generated
   source in the tree.
What stays refused, by name: a Rust type deriving from a C++ class, a Rust generic instantiating
a C++ template for an argument not yet known, and a C++ type whose layout depends on a macro the
import did not set.

Order: after C imports work on tempo's six crates (the order below), step 1 here, on the same
record and the same driver.

## Proof on real libraries (the acceptance ladder)

The owner, 2026-11-20 19:46: "the cpp thing or c thing should be tested with stuff like numpy".
Rules 7 and 82: an import is judged on real headers people use, the day it runs. A model front
end proves the machine; only these prove the feature. Every rung is a header set already on this
machine (read-only), an oracle, and a count that is printed: declarations imported, declarations
left out by cause, layouts asserted.

| rung | headers (where) | why this one | oracle |
|---|---|---|---|
| C1 | `zstd.h`, `zlib.h`, `lz4.h` (the `*-sys` crates' sources in the cargo cache) | small, clean C APIs | every declaration matches the `bindgen` output those crates ship or generate; a round trip (compress, decompress) runs from Rust |
| C2 | `sqlite3.h` (libsqlite3-sys's bundled amalgamation) | 300 functions, callbacks, opaque handles, varargs; turso's own surface | open a database in memory, run a statement, read a row; the amalgamation's one 250,000-line unit compiles as a unit task |
| C3 | `rocksdb/c.h`, `mdbx.h`, `blst.h`, aws-lc's headers (tempo's six) | what tempo needs; assembly beside C | tempo's crates build with no build script run for C |
| C4 | `Python.h` and `numpy/arrayobject.h` (`/Volumes/Projects/ref-c-corpus`: CPython 3.13's headers, numpy 2.5.5's wheel headers, fetched by digest) | the hard real case: thousands of macros, function-like macros that ARE the API (`PyArray_DATA`, `Py_INCREF`), static inline functions, a function table reached through a pointer (`PyArray_API`), bitfields, version-conditional layout | the count of what was left out, by cause, is the result: it sets what the import must learn next (a function-like macro with a typed expansion becomes an inline function; a static inline function is compiled by clang to a code record and called); struct layouts (`PyObject`, `PyArrayObject_fields`, `PyArray_Descr`) equal clang's `offsetof` |
| X1 | `rocksdb/db.h` (C++) | classes, `std::string`, virtual calls, the C++ a real database exposes | open, put, get through the C++ API; equal to the same calls through `c.h` |
| X2 | a header-only template library (chosen when C++ step 3 starts; Eigen or abseil's containers) | templates instantiated on demand | a `Matrix<f64, 3, 3>` product equals the C++ program's |
| X3 | Open Cascade's headers: `BRepPrimAPI_MakeBox`, `BRepAlgoAPI_Fuse`, `STEPControl_Writer` (**source not yet on disk**; fetched only on the owner's word) | the largest C++ API the import will take: handles (`opencascade::handle<T>`, an intrusive smart pointer), deep class hierarchies, thousands of headers, templates in collections (`NCollection_*`); and it gives the CAD library (D183 in end-state.md, an implicit kernel) a B-rep exchange path (STEP) with no registry dependency | build a box, fuse two solids and write a STEP file from Rust through the import; the file equals the one the same C++ program writes |

C4 is not expected to pass whole at first; it is there so "what is left out" is a number from the
worst header in common use and not an opinion. Each real-library failure gets a reduced header in
the crate's tests (the standing rule for real-project bugs).

**The harness** (crates/blaze-cc/src/ladder.rs, `process` kind tests): each rung runs the real
helper over the real headers through `driver::run_native`, with a `HostView` reading the
package's files (read only) from a directory named by `BLAZE_CC_LADDER` at build time, which
holds `blaze-cfront`, `shelf/clang`, `shelf/sdk`, `ref-c-corpus` and `registry` (links). A rung
whose helper or source is absent is skipped by name ("skipped: ... not set", "no helper at",
"source not on disk at"); none passes without running. Each prints its read counts and census,
asserts the lines its API must have (`fn ZSTD_compress`, `record 0 z_stream_s`, `macro str
SQLITE_VERSION`, `fn PyArray_NDIM`, `macro slot PyArray_Type`, `macro addr Py_None`, ...), a
least count of functions and constants, and for a compile rung (zstd's `entropy_common.c`,
zlib's `adler32.c`, sqlite3.c) a Mach-O with `MH_SUBSECTIONS_VIA_SYMBOLS` or an ELF with
`.text.` sections. Rungs written: zstd, zlib, lz4, sqlite, rocksdb `c.h`, blst, aws-lc `sha.h`,
c-kzg, cpython, numpy. None has run: the helper is not built.

**C4 measured (2026-11-20, a grep census, not the import):**

| | numpy 2.5.5 (`_core/include/numpy`) | CPython 3.13 (`Include`) |
|---|---|---|
| headers | 26 | 264 |
| object-like `#define` (all / a numeric literal) | 1429 / 212 | 2618 / 1583 |
| function-like `#define` | 241 | 1042 |
| of those: `##` / `do {` (statements) | 9 / 13 | 19 / 16 |
| of those: one call of another name (alias) | 68 | 322 |
| of those: a wrapper of a same-named function (`#define Py_INCREF(op) Py_INCREF(_PyObject_CAST(op))`) | 0 | 60 |
| `static inline` functions | 151 (161 with `NPY_FINLINE`) | 1002 (1010) |
| table slots through a pointer (`PyArray_API[n]`, `PyUFunc_API[n]`) | 303 + 44 | 0 |

`PyArray_NDIM` and `PyArray_DATA` are static inline (ndarraytypes.h:1594, 1600); `Py_INCREF` is
a static inline (object.h:794) behind a same-named macro; `PyArray_API` is `static void **` in
the header (__multiarray_api.h:644), `extern` under `PY_ARRAY_UNIQUE_SYMBOL`.

The two C4 designs:
- **(A) A static inline is a function compiled by clang itself.** The dump gives it as a `fn`
  of internal linkage with its body (model.rs `Linkage::Internal`, `body`); the import's unit
  (one per import, compile mode over `IMPORT_MAIN`) is compiled with an exported thunk per used
  inline, and the thunk is a code record Rust calls. Honest: the semantics are clang's exactly,
  macros inside the body included; nothing is re-expressed. This covers 1002 + 151 functions and,
  through the same-named wrappers, the 60 `Py_INCREF`-shaped macros (the function is called; the
  macro's cast is the caller's type).
- **(B) A function-like macro as a generated inline C function.** Honest only when the import
  is told its signature (a macro has no types; blaze-c's grammar `MacroFn` is where it is given):
  the generated `static inline R name(P p) { return MACRO(p); }` then takes design A's path.
  Without a signature it stays a `FunctionLikeMacro` finding; one that pastes or assigns (cause
  37) or is a fragment (15) never becomes a function. The table slots are not macros to turn into
  functions: they are `MacroValue::Slot` (the dump's `macro slot`), read through the table
  variable at run time.

## What is an error
- A header or source outside the package and its declared include paths: refused, naming it.
- A construct the import cannot express: a finding at the import, naming the declaration.
- A toolchain record that is missing: an error naming it and how it is cut; never the machine's.
- Two front ends that disagree on a header: the differential test fails; neither is preferred.

## Order
1. `crates/blaze-c`: the declaration model, its record, its rendering to Rust, the import's key
   and read list, the `CFrontEnd` seam, the macro's grammar: sans-IO, on D170's conventions,
   proven with a model front end. No toolchain needed.
2. The toolchain side: what of toolchain-c.md is built; the driver that runs the pinned clang
   natively in its own process over the roots blaze gives it and collects its reads; the first
   real `CFrontEnd`.
3. The built-in macro wired into expansion; the first real import (tempo's rocksdb `c.h`),
   compared with what bindgen writes for the same header.
4. Units as tasks and code records; the symbol index; `run` on a C function.
5. The C mode and the `bindgen` stand-in for existing crates; tempo's six crates with no build
   script run for C.

## The toolchain side (Order step 2; D174)

### What exists (2026-11-20, read in this tree)
| What | Where | State |
|---|---|---|
| The LLVM blaze links: fork, 23.1.4 | pins/llvm.json; .github/scripts/llvm.py `COMPONENTS` (line 70) | Builds clang, lld, llvm-ar, libclang into blaze's archive; no clang static libraries or headers in it |
| The C toolchain build scripts run with | pins/clang.json | The LLVM project's own 23.1.4 release, macOS and Linux arm64, by sha256; sysroots unpinned (null) |
| The clang shelf | `<home>/.blaze/v2/toolchain/clang/<sha256>/`; blaze-build-script toolchain.rs `SHELF_MEMBERS` (201) | Unpacked release; the `clang.cfg` files `SHELF_MEMBERS` names are written by no code found |
| The env a C compile gets | toolchain.rs `c_env` (234), `depfile_reads` (276) | `CC`/`CXX`/`AR` and flags for a build script's `cc` crate; the reads are what clang's depfile says |
| C tools a script may start | blaze-build-script probe.rs `C_TOOLS` (53); native.rs `TOOLCHAIN_CLASS` (23) | A script whose closure holds bindgen, cc, clang-sys, cmake or pkg-config runs natively in Seatbelt |
| How a spawn of cc is routed | blaze-proc-macro spawn_route.rs `route` (152); key.rs `CMode` (29) | Per name: run under the grant, or refused |
| Crates native in every mode | toolchain.rs `NATIVE_EVERY_MODE` (356) | libproc, librocksdb-sys, reth-mdbx-sys, sha3-asm |
| A served child | blaze-std procs/linked/run.rs `Spawner::start_served` | A child with a message channel on fd 3, a deadline on every wait; no sandbox field |
| OS confinement | blaze-core lib.rs `Confine` (162) | A trait with no shipped implementation |
| A checked-in source correction | blaze-fetch correct.rs, applied in blaze-world list.rs (188) | D58: the precedent for the table in "Existing crates" below |
| The cut | scripts/src/bin/toolchain_cut.rs `place` | Cuts the Rust shelf; now also places blaze-cfront (`cfront`) |

The 24 s: tempo-deps profile (port27, engine-g004958) line 168, `24439.3 ms librocksdb_sys`.
librocksdb-sys is in `NATIVE_EVERY_MODE`, so a check runs its whole script: by its source, the
`cc` compile of rocksdb's about 330 C++ files, then bindgen. libproc's script, bindgen alone,
is about 0.3 s. So the 24 s is most likely the C++ compile, which a check never needs; this is
read from the profile's one row and the script, not measured by span.

### The decision: a helper linked to clang, not the clang binary
blaze runs the pinned clang natively, in its own process, as **blaze-cfront**: a small C++
program linked to the pinned build's clang libraries (crates/blaze-cc/helper, built by llvm.py
with `"cfront": true`, pinned in pins/cfront.json, put on the shelf by toolchain_cut as
`<shelf>/cfront/<sha256>/blaze-cfront`). It runs clang's own driver and its cc1 job in process,
over a virtual file system whose only source is blaze: each stat or open is a message on bstd's
served channel, answered from the roots, and so recorded, found or missing. It is one process per
task: a crash fails that task (rule 20).

What it buys over the binary:
- an exact read list, misses included, with no depfile and no trace;
- guest paths by construction (`/package/...`), so no prefix maps and no host path in an object;
- nothing written out: a package or the SDK is served from records, never unpacked into a folder;
- one tool for the declaration dump and the compile, with a dump made for blaze (below);
- confinement that is simple: it needs no file at all.

What the plain `clang` binary would buy: it exists today and needs no C++ of blaze's to keep up
with clang's API; its behaviour is upstream's exactly. Its costs: a read list from the depfile
(no misses: the include search's failed lookups are lost, so a header added earlier on the path
is not seen) or from a trace; real directories to read from; prefix maps for every host path;
for the dump, clang's JSON AST, which is unstable, carries no layouts and is hundreds of MB for
one header. And its roots can be held only by the OS sandbox. The helper's maintenance is about
one file against clang's Driver, FileSystem and RecursiveASTVisitor APIs, which move little
between majors; blaze moves its LLVM pin rarely and on purpose.

### Hermetic by construction
- **Roots** (crates/blaze-roots, used directly; the interim copy is gone): `Roots::new` over the
  clang tree (resource headers, libc++ headers) and the sysroot tree (`Tree` roots), the package
  with its declared globs (`Package`), one output (`Output`, never read). `Mounts` spells each as
  a guest path, `/<mount>/<relative path>`; `resolve` drops `.`, takes `..` by the text alone
  (the trees hold no links), refuses above `/` and an unmounted path (`Unmounted`). The job admits
  each lookup before it is answered (`admits_file`, `admits_dir`), and the read list is
  `ReadList::under`'s `Checked` proof. What blaze-roots still lacks for this crate: an owned
  `RelPath` (the job keeps a `LookPath` string wrapper beside blaze-c's `RelPath`: two homes for
  one concept, rule 30), a planted-bug home in `Mounts` (the job plants `DropsDotDot` in its own
  copy of the walk), and a root kind for the SDK read from the machine (decision 1: it is a
  `Tree` with its digest computed at the read, not a record).
- **Environment**: cleared. Arguments: exactly `flags::args`, listed with why in
  crates/blaze-cc/src/flags.rs: `--no-default-config` (no `clang.cfg` from beside the
  binary), `-no-canonical-prefixes`, the target and `-isysroot /sdk` or `--sysroot=/sdk`,
  `-fdebug-compilation-dir=/package` and the coverage twin, `-gno-record-gcc-switches`, no
  colour, `-fmessage-length=0`, `-fno-crash-diagnostics` (no reproducer written to a temporary
  folder), `-ffunction-sections -fdata-sections -fno-common -fPIC` (checked in flags.rs at
  lines 209-216; on Mach-O clang sets `MH_SUBSECTIONS_VIA_SYMBOLS` itself, which the ladder's
  compile rungs assert in the object's header). `__DATE__`, `__TIME__` and `__TIMESTAMP__` are
  fixed by `-D` (flags.rs `FIXED`); there is no clock.
- **The OS hold, before the first message** (helper/main.cpp `confine`): macOS, `sandbox_init`
  with `(version 1)(deny default)(allow sysctl-read)(allow signal (target self))` (a private
  API, rule 45; the ladder's rungs are the test that its symbol exists, as a Hello saying
  Seatbelt); Linux, `PR_SET_NO_NEW_PRIVS`, a Landlock ruleset handling every file right with no
  rule (where the kernel has one), then a seccomp filter returning `EPERM` for `open`, `openat`,
  `openat2`, `creat`, `open_by_handle_at`, `socket`, `socketpair`, `connect`, `bind`, `execve`,
  `execveat`, `fork`, `vfork` and `ptrace`. The helper reports which in `Hello`; blaze refuses
  less than the system offers (`Unconfined`).
  So the OS roots of the helper are nothing, and the read list checked against the roots
  (`ReadList::under`, a `Checked` proof) is the check of what it saw.
- What the base crates lack: bstd's `RunSpec` has no sandbox field (the helper confines
  itself), and `RunChild::recv_message` takes `alloc`'s `Vec`, not blaze-own's.

### The wire (crates/blaze-cc/src/wire.rs and helper/wire.h, version 2, big-endian)
Each message is a 4-byte length, then the body, on the served channel (fd 3). From the helper:
`Hello{version, confinement}`, `Look{id, op: stat|read|list, guest path}`, `Out{object|decls,
bytes}` (pieces of at most 1 MiB), `Done{errors}`, `Diagnostic{level, clang's id, offset, guest
path, message}` (one row each; version 1 sent diagnostics as text). From blaze: `Job{mode:
decls|compile, args}`, `Answer{id, missing | file{size, bytes when read} | dir | listing{sorted
names, a directory's with `/`} | not a file}`. A listing's digest is of what was sent, each name
admitted by its root; a read that meets a directory is a directory seen (clang opens a header
candidate without a stat first). The file's digest is blaze's (the driver hashes what it sends);
it is not on the wire.

### The helper (crates/blaze-cc/helper: CMakeLists.txt, wire.h, main.cpp)
- **Linked**: clangFrontend, clangCodeGen, clangDriver, clangParse, clangSema, clangAnalysis,
  clangAST, clangASTMatchers, clangEdit, clangLex, clangBasic, clangSerialization,
  clangSupport; LLVM's AArch64 and X86 targets, Core, Support, Option, MC, TargetParser,
  BitWriter, IRReader, Passes; libsandbox on macOS. Statically; CMake refuses an LLVM other
  than 23.
- **The file system**: `WireFS`, an `llvm::vfs::FileSystem` with nothing under it: `status`,
  `openFileForRead`, `dir_begin` are Looks (answers cached per path for the job), `getRealPath`
  is lexical, the working directory is `/` only, `isLocal` is false, a status has no time and
  no owner and a unique id per path. `IMPORT_MAIN` and `/.blaze` are the helper's own. The nine
  files clang's Linux driver reads to name the host's distribution (`/etc/os-release`, ...)
  are answered missing without a Look: the guest has no `/etc`, and they are not under a mount,
  which blaze would refuse as `Unmounted` and so fail every Linux job.
- **The hand-off**: `driver::Driver` over `WireFS` turns blaze's arguments into exactly one
  `clang -cc1` command (else the helper exits with status 5), and `CompilerInvocation::
  CreateFromArgs` makes the invocation each parse copies.
- **Decls mode**: two parses over one `WireFS` (the second reads no file again). The first is
  the preprocessor alone to the main file's end; it collects every macro the headers left
  defined (not builtins, the command line or header guards), sorted by place, and sorts out
  the function-like (cause 0, or 37 when it pastes or assigns), the empty (28) and the pasting
  object-like (37). The second parses with a main file of probes, `static void
  __blaze_probe_N(void) { (void)(NAME); }`, one per object-like macro left, after every header,
  so each macro is read by clang as it is at the end; diagnostics about the probes are dropped.
  The walk (`Dumper`) writes a line per declaration in the TU's order, types interned by
  `QualType`, a record's layout from `getASTRecordLayout`, each probe's expression classified as
  `str`, `addr`, `slot`, `int`, `float` (by clang's evaluator) or left out (`MacroNotConstant`,
  `MacroIsFragment`, `WideString`, `LongDouble`).
- **Compile mode**: `EmitObjAction` with an `llvm::vfs::OutputBackend` that keeps the one `.o`
  in memory (any other output is refused); sent as `Out{object}`.
- **Diagnostics**: a `DiagnosticConsumer` sends each as a row, the path and offset by the
  location's file place.
- **Nothing ambient**: no environment read, no clock, no thread, descriptor 3 only; the exit
  status names the class of a stop (2 wire, 3 confinement, 4 job, 5 driver, 6 front end).

### The machine (crates/blaze-cc/src/job.rs) and the driver
`Job::new(JobOpts)`, `step(now, Event)`, `next_effect`, `next_deadline`, `state`, `finish`: it
spawns, checks the greeting, admits each lookup against the roots before it is answered
(`NotUnderRoot` otherwise), caps looks, file sizes and output, and ends in `Finished{errors,
object, decls, diagnostics, reads: Checked<ReadList>}` or a typed `JobError` (a crash names the
signal and the phase). driver.rs `serve` runs it over any `RunChild` with a `View` answering
lookups from records; `run_native` starts the shelf's helper with no environment, all streams
null, its own group and the strongest parent-death signal. Planted bugs (forgets misses, drops a
`..`) are caught by the simulation's oracle.

### The declaration dump: `cfront-decls 2`
Not clang's JSON AST: a line format of blaze's (crates/blaze-cc/src/decls.rs), one line per
call of blaze-c's `DeclsBuilder`, so the model (crates/blaze-c/src/model.rs) is the one
statement of what a front end gives and the format is its spelling; ids are positional. Its
lines and the model's columns:

| line | model call | columns |
|---|---|---|
| `abi` | `DeclsOpts.abi` (checked equal) | pointer size, alignment, endianness, char signedness |
| `header` | `header(root, path)` | guest path (hex), resolved to a root by the mounts |
| `ns` | `namespace` | name, parent |
| `ty ...` | `ty(CType)` | void, bool, int (kind, size, align, signed, checked against the `CAbi`), float, ptr, ref, array, fn (result, variadic, prototyped, convention, params), record, enum, typedef, valist (lowered), unexpressible (cause, size, align); qualifiers |
| `record` / `define-record` + `unit` `field` `base` `end` | `declare_record` / `define_record(RecordBody)` | kind, name, ns, origin / size, align, pack, relocation; units (offset, size); fields (name, type, bit offset, width, unit, range); bases (record, offset, virtual) |
| `enum` / `define-enum` + `const` `end` | `declare_enum` / `define_enum` | name, ns, origin / underlying, complete, scoped; constants (name, bits, range) |
| `typedef` | `typedef` | name, ns, type, origin |
| `fn` | `function(Function)` | name, ns, symbol, type, attributes, owner, method kind, linkage, body, origin, parameter names |
| `var` | `variable(Variable)` | name, ns, symbol, type, attributes, linkage, init (absent, zero, other, bits), origin |
| `macro int` `float` `str` `slot` `addr` | `constant(MacroValue)` | Int{bits, ty}, Float{value, ty}, Str{bytes, ty}, Slot{table, index, ty, deref}, AddressOf{variable} |
| `unexpressed` | `unexpressed` | name, ns, decl kind, cause, origin |

Every column the model has is in the format. What the model lacks (so the dump cannot say):
a typedef's own alignment attribute; availability, `weak_import` and visibility; a
function-like macro's signature (design B); a pointer-valued constant (`((void *)0)`,
`SQLITE_TRANSIENT`), left out as `MacroNotConstant`; a record clang made itself (AArch64
Linux's `struct __va_list`), which has no header, so `__builtin_va_list` lowers to an
unexpressible row there. `DeclsReader` is a codec (`feed` → `Progress`, then `finish`), proven
invariant under how the bytes are split.

### The front end (crates/blaze-cc/src/front.rs `ClangFrontEnd`)
blaze-c's `CFrontEnd` over one decls `Job`: each Look becomes an ask of the import (a place's
search is one folder), so the import's read list holds every probe clang made, present or
absent. It lives in blaze-cc because blaze-c depends on base crates alone. What the seam lacks,
met in front.rs and to be moved into blaze-c: a directory ask and a listing (a stat of a root
is a directory, any other missing stat `NotAFile`; a listing is refused, `TOOL_LIST`); a typed
fault for the tool (the job's refusals arrive as diagnostics with codes at or above `TOOL`); a
separate SDK root; one `RelPath` for both crates; one read list (the job's duplicates the
import's); and the header bytes are hashed twice (the driver for the read list, the import for
its key).

### What a compile hands to code records
A relocatable object (Mach-O on macOS, ELF on Linux) with a section per function and per datum,
so a code record can take one function; the checked read list; the diagnostics as rows (level,
clang's id, root, path, offset, message), which become findings records keyed like the unit. The unit's key is the flags, the toolchain's id
and the roots' digest; the record stays valid while the read list holds.

### Existing crates (design only)
- **A `cc`, `c++` or `ar` spawn is a unit task.** spawn_route's C mode, instead of running the
  machine's or the shelf's binary, parses the `cc` crate's command line into a `flags::Unit`
  (the source, `-I`, `-D`, the standard, the opt level); a flag it does not know is refused by
  name, never passed through. The unit is compiled by blaze-cfront as a task of its own, and the
  script is answered with the object at the path it asked; `ar` is answered by blaze writing the
  archive from those objects. In check mode, an object no Rust item links is never compiled:
  this is what removes rocksdb's 24 s.
- **"Provided by blaze".** A checked-in table, modelled on D58's corrections, of packages blaze
  supplies itself, by name and version range: first `bindgen` (the range is the versions tempo's
six crates lock, read when the row is written), answered by
  blaze's import over `cfront-decls`, rendering what bindgen's builder asked. Each row names the
  package, the range and what replaces it; `blaze why <crate>` prints the row for any crate it
  touches; a version outside the range is refused by name, never answered silently.

### Decisions (2026-11-20, from the coordinator)
1. **The macOS SDK** stays open for the owner (whether blaze may ship its headers and `.tbd`
   stubs as records). Until he answers, the machine's SDK is read by its version, and its
   digest is in the key: a different SDK is a different key, never the same one.
2. **The helper's C++ is accepted** as blaze's one piece of non-Rust code, and it holds only
   the file-system adapter, the argument hand-off and the declaration dump. Anything that
   decides (admission, the read list, limits, the model) is Rust, in job.rs and blaze-c.
3. **The Linux sysroot** is a pinned archive, by digest. A sysroot blaze builds from a pinned
   glibc comes later.
4. **llvm.py may include clang's static libraries** for the helper's build (CFRONT_COMPONENTS);
   they are removed from the archive after it, so blaze's archive does not grow by them.

### UNSURE
- The clang API in main.cpp is from memory of clang 20-22, not read from clang 23's headers
  (none were on the machine); the end of main.cpp lists each name relied on. The likeliest to
  have moved: `CompilerInstance::createDiagnostics`'s arguments, `setOutputBackend` and whether
  `createDefaultOutputFile` writes through it (else `EmitLLVMOnlyAction` and
  `emitBackendOutput` into memory), `DiagnosticsEngine`'s constructor, `TypedefType::getDecl`.
- Bitfield storage units are the declared type's aligned unit (what a C reader of the field
  loads), not CodeGen's access units; equal on the SysV and AAPCS layouts the rungs use, by
  reading, not measured.
- Whether Seatbelt's `deny default` lets the helper keep reading and writing descriptor 3 (an
  open descriptor needs no file right, by reading); the first rung that runs says.
- Whether the job's deadline and the child's clock share a base (ladder.rs takes the world's
  clock).
