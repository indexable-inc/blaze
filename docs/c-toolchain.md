---
status: plan
summary: blaze's own pinned C toolchain and sysroots for the C and C++ that build scripts compile, never the machine's; wasm32 first, then Linux, macOS and Windows.
date: 2026-10-30
---
# blaze's own C toolchain

blaze compiles the C and C++ that build scripts compile (`cc`, `cmake`, `autotools`, a
`Makefile`) with a C toolchain it pins and fetches itself, as it does its Rust toolchain
([toolchain.md](toolchain.md)): a clang, its archiver and linker, and a sysroot per target with
the target's libc headers and libraries. The machine's `cc`, `clang`, `/usr/include` and SDKs
are not used for a target blaze pins a toolchain for. Andrew approved this on 2026-10-30.

Until now the C toolchain was outside the rules ([no-rustc.md](no-rustc.md) "What it does not
cover", [cargo.md](cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox),
[linker.md](linker.md)): a build script ran whatever `CC` and `PATH` gave it. So one project's
build depended on the runner. The gate to stable failed zed on all three of its runners
([run 40968467065](https://github.com/indexable-inc/blaze/actions/runs/40968467065)): blaze
builds a proc macro as a WebAssembly module ([cargo.md](cargo.md#proc-macros-run-as-webassembly)),
so it runs `zstd-sys`'s build script for `wasm32-wasip1`, and cc-rs ran the runner's `clang`:

- On ubuntu-24.04 (x86-64 and arm64), clang 18 with no wasi sysroot searched `/usr/include`,
  glibc's headers for the host, and stopped at `'bits/libc-header-start.h' file not found`.
- On macos-15, Apple's clang has no WebAssembly backend: `No available targets are compatible
  with triple "wasm32-unknown-wasip1"`.

rustc has the same hole and leaves it to the user (cc-rs reads `WASI_SYSROOT`; the user installs
wasi-sdk). blaze chose the target here, so the C toolchain for it is blaze's to give.

## Prior art

- **zig cc.** One download has clang, lld and the libc headers of every target zig supports
  (glibc, musl, mingw-w64, wasi-libc, macOS's headers); glibc's shared libraries are stubs zig
  generates from its ABI lists for the glibc version named in the target
  (`x86_64-linux-gnu.2.28`), and libc++, compiler-rt and musl are built from source on first use
  and cached. Any host cross-compiles to any target. The macOS headers are extracted from Apple's
  SDKs, which zig redistributes under a reading of the licence blaze does not take (below).
- **Bazel hermetic toolchains.** `toolchains_llvm` and `hermetic_cc_toolchain` (zig cc) fetch a
  clang and a sysroot per target as repositories pinned by sha256, and register them so that no
  action sees `/usr`. A sysroot is an archive the user or the rule set names.
- **Nix's cc-wrapper.** The compiler is a wrapper that adds the closure's headers and libraries
  (`NIX_CFLAGS_COMPILE`, `NIX_LDFLAGS`), with `NIX_ENFORCE_PURITY` refusing `-I/usr/include` and
  the like. blaze already runs build scripts in such a closure on Linux when
  `BLAZE_SCRIPT_VIEW=on` (`crates/blaze-cargo/src/cargo/hermetic/mod.rs`): clang 21, lld, glibc
  2.44 and the kernel headers, pinned from nixpkgs in
  `crates/blaze-cargo/src/cargo/closure/pins.txt`.
- **rustc's self-contained linking.** `rust-std` for `wasm32-wasip1` ships wasi-libc's `libc.a`
  and start files in `lib/rustlib/wasm32-wasip1/lib/self-contained`, built from wasi-sdk 34.0 at
  the pinned nightly (rust-lang/rust `src/ci/docker/host-x86_64/dist-various-2/Dockerfile`), and
  the musl targets ship musl's; `-Clink-self-contained=+linker` links with the shipped `rust-lld`.
  rustc ships no compiler or headers for C.

## What is pinned

One clang per host compiles for every target; each target gets a sysroot. Where a piece cannot be
pinned, the build stops and names it, as below.

| target | compiler | sysroot | where it comes from |
|---|---|---|---|
| `wasm32-wasip1`, `wasm32-wasip1-threads`, `wasm32-wasip2`, `wasm32-wasip3` | wasi-sdk's clang, `llvm-ar`, `llvm-ranlib` | wasi-sdk's `share/wasi-sysroot` | wasi-sdk, the release the pinned nightly builds its wasi-libc with (34.0), by sha256 |
| `wasm32-unknown-unknown`, `wasm32v1-none` | the same clang | none: clang's own freestanding headers | the same download |
| `*-unknown-linux-gnu` | clang | glibc headers and libraries of one glibc version, the kernel's headers, libgcc | to decide (below) |
| `*-unknown-linux-musl` | clang | musl's headers, from the musl `rust-std` ships the `libc.a` of | musl's source by sha256, headers installed by blaze |
| `*-apple-darwin` | clang | the Apple SDK installed on the Mac, at the version blaze names | Xcode or the Command Line Tools: blaze cannot ship it |
| `*-pc-windows-msvc` | `clang-cl`, `lld-link`, `llvm-lib` | the MSVC CRT and the Windows SDK, at the versions blaze names | Visual Studio's install: blaze cannot ship them |
| `*-pc-windows-gnu`, `*-pc-windows-gnullvm` | clang | mingw-w64's headers and import libraries | mingw-w64 by sha256 |

### The compiler

LLVM's clang is the one compiler for every target, as with zig: one program cross-compiles, and
its version is blaze's, not the runner's.

The LLVM blaze pins (`crates/hir-ty-codegen/llvm.json`, LLVM 23.1.4 with our fixes) cannot serve
yet: `.github/scripts/llvm.py` builds no clang or lld (`LLVM_ENABLE_PROJECTS` is empty), only
the X86 and AArch64 targets, and installs the libraries `--features llvm` links, not tools. The
long-term compiler is that LLVM with `clang;lld` added to its projects, WebAssembly added to its
targets, and a second archive per host from the same build with `clang`, `clang++`, `clang-cl`,
`lld`, `llvm-ar`, `llvm-ranlib`, `llvm-nm`, `llvm-objcopy` and clang's resource headers, pinned
in `llvm.json` beside the libraries. One LLVM then compiles blaze's code, assembles `asm!`
([no-rustc.md](no-rustc.md), step C) and compiles C. Moving the pin is the `llvm` workflow
already ([contributing.md](contributing.md#llvm-for---features-llvm)).

Until then each step takes the clang of the download its sysroot comes from, so a step does not
wait on a five-hour LLVM build: wasi-sdk's for wasm32. When blaze's own clang lands, the wasm32
step keeps only wasi-sdk's `wasi-sysroot-<v>.tar.gz` and `libclang_rt-<v>.tar.gz`, which it
publishes on their own for this.

### wasm32

wasi-sdk is Apache-2.0 with LLVM's exception (clang, lld, libc++), with wasi-libc under MIT,
Apache-2.0 and BSD (musl's part MIT); it may be redistributed. blaze downloads it from its GitHub
release by the sha256 GitHub publishes, so blaze redistributes nothing.

Its version is the one the pinned nightly builds `rust-std`'s wasi-libc with, so the headers a C
file is compiled against are the headers of the `libc.a` the module links. The daily bump checks
that and moves it with the nightly.

`wasm32-unknown-unknown` and `wasm32v1-none` have no libc. They get the same clang with no
sysroot, so a C file sees clang's freestanding headers (`stdint.h`, `stddef.h`, `stdarg.h`) and
nothing of the host. A crate that needs more ships it itself, as `zstd-sys`'s `wasm-shim` does.

### Linux

The pieces are the ones `BLAZE_SCRIPT_VIEW` already gives from nixpkgs (glibc, the kernel's
headers, libgcc, clang and lld), and blaze's linker already finds the start files, `libgcc` and
libc from the C toolchain's search directories ([linker.md](linker.md)), so one sysroot serves
compiling and linking.

To decide: which glibc. A program is linked against the glibc of its sysroot, so the glibc
version decides where the program runs and which C functions a build script's C may call.
Rust's own builds target glibc 2.17. Choices:

1. The newest (nixpkgs', 2.44 today): everything that compiles on any runner compiles, and the
   program runs only where glibc is that new or newer.
2. One old glibc (2.28, RHEL 8 and manylinux_2_28): the program runs almost everywhere, and C
   that calls a newer function (`getrandom`, 2.25, is older; `pidfd_open`, 2.36, is not) fails
   to link where rustc and the machine's glibc would link it.
3. zig's way: headers of the newest glibc, stub libraries per version, the version a setting of
   the target with one of the above as the default.

The third is the long-term design; the default is Andrew's decision. musl is simpler: its
headers are installed from the musl source of the release `rust-std` ships (`make
install-headers` needs only `make` and `sed`), and `libc.a` and the start files are `rust-std`'s
self-contained ones.

### macOS

Apple's SDK may not be redistributed: the Xcode and Apple SDKs Agreement licenses it for use on
Apple-branded computers and grants no right to distribute it. So blaze does not ship it, and does
not take zig's extracted headers either. On a Mac, blaze takes the SDK the Command Line Tools or
Xcode installed, as `xcrun --show-sdk-path` finds it, and checks its version
(`SDKSettings.json`) against the one blaze names. The compiler is blaze's clang with `-isysroot`
of that SDK and `-mmacosx-version-min` of rustc's deployment target (11.0 on arm64), never Apple's
clang. A Mac without the SDK, or with another version, stops with the version wanted and the
command that installs it. Other Apple tools a build script runs (`xcrun metal`, `actool`) are
not the C toolchain and stay the machine's.

Cross-compiling to macOS from Linux or Windows has no SDK and stops, naming it.

### Windows

The MSVC CRT and the Windows SDK may not be redistributed either. On Windows, blaze locates
Visual Studio's install as cc-rs does (`vswhere`), checks the versions against the ones blaze
names, and runs its own `clang-cl` and `lld-link` against them. A machine without them stops
with what to install. Downloading them from Microsoft's manifests after an explicit acceptance of
Microsoft's licence (as `xwin` does) is a later opt-in, never the default. `windows-gnu` and
`gnullvm` use mingw-w64, which may be redistributed. Build scripts do not run on Windows until it
has a sandbox ([#257](https://github.com/indexable-inc/blaze/issues/257)).

## How it is fetched

As blaze's Rust toolchain is ([toolchain.md](toolchain.md)): what blaze downloads is pinned in
`crates/blaze-cargo/src/cargo/toolchain/c_pins.txt`, a line per host with the file's name, its
sha256 and its size. `harness/c_toolchain_pins.py` writes it: the wasi-sdk version from the
nightly's commit (the Dockerfile above), the digests from GitHub's release, and checks the file
with `{}`. A download is checked against its sha256, unpacked into a staging directory and moved
into `~/.blaze/c/<name>-<host>-<first 16 of the sha256>` whole, under a lock, once per machine;
processes that ask at once download it once. `BLAZE_C_DIST` names a mirror or a directory with
the files, for a machine with no network. A pin moving makes a new directory and the old one is
removed, as old nightlies are.

## How it reaches a build script

Every run of a build script, cargo's and the daemon's, sandboxed or not, has its environment made
in one place (`script_sandbox.rs` `plan`). There blaze sets, for the script's `TARGET`, the
target-specific variables cc-rs, `cmake` and their kin read first:

| variable | value for `wasm32-wasip1` |
|---|---|
| `CC_wasm32_wasip1`, `CXX_wasm32_wasip1` | the pinned `clang`, `clang++` |
| `AR_wasm32_wasip1`, `RANLIB_wasm32_wasip1` | the pinned `llvm-ar`, `llvm-ranlib` |
| `CFLAGS_wasm32_wasip1`, `CXXFLAGS_wasm32_wasip1` | `--sysroot=<the sdk>/share/wasi-sysroot` |
| `WASI_SYSROOT` | the same directory, which cc-rs also reads for a wasi target |

cc-rs takes `CC_<target>` before `TARGET_CC` and `CC`, so the host's `CC` and `CFLAGS` still
compile for the host and none of them reaches the wasm32 compile. The pinned directory is made
readable in the sandbox when it would be empty there (`BLAZE_HOME` below `/tmp`, as in the gate),
and is seen in a run of `BLAZE_SCRIPT_VIEW=on`. The variables name the pinned directory, whose
name has the download's sha256, so the run's key in the store has the toolchain in it.

## What a user may set

`BLAZE_C_TOOLCHAIN=given` is the one opt-out: blaze sets none of the variables above, and the C
toolchain is what the environment gives, as cargo has it. It is a setting of the run, so it is in
the run's key.

Without it, a variable blaze would set that the environment already sets for that target
(`CC_wasm32_wasip1`, or cc-rs's other spelling `CC_wasm32-wasip1`, `WASI_SYSROOT`, ...) stops the
build: blaze names the variable and the two ways out, unsetting it or `BLAZE_C_TOOLCHAIN=given`.
It is never quietly replaced, and never quietly honoured.

## When a piece is missing

- The download fails or its sha256 differs: the build stops, naming the file, the address and
  what to do (run again, or `BLAZE_C_DIST`). The machine's compiler is not used instead.
- A host blaze pins nothing for: the build stops naming the host and the target.
- A target whose step has landed with no pinned piece for it (a Linux target with no sysroot, a
  Mac without the SDK version): the build stops naming the target and the piece.
- A target whose step has not landed yet compiles with the environment's toolchain, as before;
  this doc's [steps](#steps) list which, and the docs that say the C toolchain is outside the
  rules say which targets still are.

## Steps

Each lands as its own pull request with its tests.

1. **wasm32**, what breaks the gate: wasi-sdk 34.0 pinned for the four hosts blaze ships,
   fetched once, given to every build script of a `wasm32-wasi*` target, the opt-out and the
   error above; then zed's gate run.
2. **wasm32 freestanding**: `wasm32-unknown-unknown` and `wasm32v1-none` with the same clang and
   no sysroot.
3. **blaze's clang**: `llvm.py` builds clang and lld with the WebAssembly target, a tools archive
   per host pinned in `llvm.json`; wasm32 moves to it with wasi-sdk's sysroot archive alone.
4. **Linux**: the glibc decision, the glibc and musl sysroots, the error for a Linux target
   without one.
5. **macOS**: the SDK check, blaze's clang with it.
6. **Windows**: mingw-w64 for `windows-gnu`; the MSVC check, `clang-cl` and `lld-link`.
