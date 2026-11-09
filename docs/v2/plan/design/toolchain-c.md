---
status: plan
summary: blaze uses only its own pinned LLVM and ships one pinned C toolchain built from it (clang, lld, compiler-rt, libc++, wasi-libc, libc stubs), so no compiler, sysroot, SDK or runtime the machine installed changes what blaze builds; rustup and the nightly download go.
date: 2026-11-02
---
# One pinned C toolchain, built from blaze's LLVM

Andrew, 2026-11-02: blaze should use only OUR pinned LLVM, never toolchain pieces the OS
installed. And: always force our standard library in `./library`. blaze is its own toolchain
(rule 6, [toolchain.md](../../../toolchain.md)) for Rust already; this plan finishes the job for
everything under Rust: the C compiler, the assembler, the linker's inputs, the runtimes and the
SDKs. The rule it serves is rule 6's: what blaze builds depends on the blaze version, not on what
happens to be installed. Rule 3 (no fallback) shapes every slice: a missing pinned piece is an
error naming it and how to get ours, never a quiet use of the machine's.

## What comes from the machine today

LLVM is pinned already: `vendor/llvm-sys/pin.rs` accepts only the prefix whose `blaze-llvm.json`
matches `crates/hir-ty-codegen/llvm.json`, built by `.github/scripts/llvm.py` and fetched by sha256
(the flake's `blaze-llvm`). The std source is pinned too: `library/`, digest-checked
(`crates/blaze-cargo/src/toolchain/library.rs`). What still comes from the machine and changes
generated code (audit of 2026-11-02, line numbers of that tree):

| # | Piece | Where | How it is found | Pinned? |
|---|---|---|---|---|
| 1 | crt objects (`Scrt1.o`, `crti.o`, `crtbeginS.o`, `crtendS.o`, `crtn.o`), `libc.so.6`, the ELF interpreter | `blaze-link` `target.rs` `linux`, `c_driver` | `-print-file-name=` of `$CC_<triple>`, `$CC` or `cc` | no: the driver is unhashed, the files it names are hashed |
| 2 | `-L`/`-F` search dirs (`NIX_LDFLAGS`, `LIBRARY_PATH`) | `target.rs` `linker_dirs` | `<cc> -### -x c /dev/null` | no |
| 3 | the macOS SDK (`SDKSettings.json`, `.tbd` stubs, frameworks) | `target.rs` `apple` | `$SDKROOT`, else `xcrun --show-sdk-path` | version read, not pinned |
| 4 | `MACOSX_DEPLOYMENT_TARGET` | `target.rs` | env | clamped to rustc's minimum |
| 5 | MSVC CRT and Windows SDK libs | `target.rs` `msvc`, `newest_kit` | `$LIB`, `vswhere -latest`, the registry's newest kit | no, floats |
| 6 | `asm!` text assembled by clang or `cc` | `hir-ty-codegen` `asm.rs` | `PATH` walk, without `--features llvm` or with `BLAZE_ASSEMBLER=cc` | no. **Gone, slice 1** |
| 7 | build scripts' `CC`/`CXX`/`AR`/`CFLAGS`/`SDKROOT`, so cc-rs runs the machine's compilers | `blaze-cargo` `serve/unit/script.rs`, `blaze-build-script` `env.rs` | inherited env and `PATH` | `RUSTC` is blaze; `CC` and `AR` are not |
| 8 | the Rust sysroot: rustup's copy of the nightly, trusted by directory name | `blaze-cargo` `toolchain.rs` `found`/`installed` | `RUSTUP_HOME` or `~/.rustup` | by name only. **Gone, slice 2** |
| 9 | the nightly release itself (blaze's own copy): sanitizer runtimes `librustc-nightly_rt.*`, wasi-libc, `rustc --print sysroot`'s answer, cargo's version | `toolchain/own.rs`; `blaze-link` `runtimes`, `wasi`; `answers/own.rs` | static.rust-lang.org, by sha256 (`pins.txt`) | pinned, but built by someone else's LLVM |
| 10 | `CC`/`CXX`/`AR`/`CFLAGS`/`SDKROOT`/`MACOSX_DEPLOYMENT_TARGET` left out of code identity on purpose | `blaze-codegen-code` `lib.rs` `unkeyed_env` | n/a | a documented gap |
| 11 | blaze's own C: the sandbox interposers | `blaze-cargo/build.rs` | `$CC` or `cc` | no |
| 12 | blaze's own C++: llvm-sys's wrappers and the C++ runtime they link | `vendor/llvm-sys/build.rs` | cc-rs, `$CXX -print-file-name=libstdc++.a`, `-lstdc++`/`-lc++`, `brew --prefix` | no |
| 13 | the compiler that builds the pinned LLVM | `llvm.py` | gcc/g++ on Linux, AppleClang on macOS, `cl` on Windows | its version is not in `blaze-llvm.json` |

Readers of the nightly's sysroot (row 9), every one of which the end state replaces:
`toolchain::sysroot` is read by the daemon at start (`serve.rs`), by `rustc --print sysroot` and
`--print target-libdir` answers (`answers/own.rs`, plus `cargo_version` from
`multirust-channel-manifest.toml`), by the sanitizer check (`serve/unit/codegen.rs` `sanitize`,
`unstable.rs` `runtimes`) and by every link (`codegen.rs` -> `blaze_link::target::locate`:
`runtimes` and `rpaths` for `librustc-nightly_rt.{asan,tsan,msan}`, `wasi` for
`lib/rustlib/wasm32-wasip1/lib/self-contained/{crt1-command.o,crt1-reactor.o,libc.a}`).
`toolchain::ensure` is called by `blaze toolchain` and before every command (`cli.rs`).
`hir-ty-codegen/stable.rs` `INSTALLED_LIBRARY` only rewrites a path string for debug info and reads
nothing. The nightly's own rlibs, `rustc` and `cargo` are not read by v2's daemon at all.

Not in scope, since they change no generated code: `git` for registry git deps (commit-pinned),
the harnesses' `llvm-dwarfdump`/`nm`/`objdump`/`readelf`, and the release's `llvm-profdata`.

## The end state

blaze ships two pinned archives per host, both built from the one LLVM commit `llvm.json` names
and fetched by sha256 like the LLVM release is today:

- **`blaze-cc-<version>-<commit>-<host>.tar.xz`, the C toolchain:**
  - `clang` (one binary, with `clang++` and `cc`/`c++` as links), its resource headers, and a
    clang config file per supported target (`bin/<triple>.cfg`, as xclang does) that sets
    `--sysroot`, `-resource-dir`, the C++ library, `-fuse-ld=lld` and `--rtlib=compiler-rt`, so a
    bare `clang --target=T` is hermetic and nothing on the machine is searched;
  - the integrated assembler, which is clang's and is also what `asm!` goes to (in process,
    through llvm-sys, since slice 1);
  - `llvm-ar` (and `llvm-ranlib`, `llvm-nm`, `llvm-objcopy`, `llvm-strip` as links of
    `llvm-objcopy`/`llvm-ar`, for the build scripts that call them);
  - `lld` (`ld.lld`, `ld64.lld`, `lld-link`, `wasm-ld`), for the links C build systems make on
    their own (CMake's `try_compile`, autoconf's checks, a `-sys` crate that links a test
    program). blaze's own links of Rust outputs stay blaze-link's, in process; lld never links
    what blaze builds;
  - compiler-rt for every supported target: builtins, `crtbegin`/`crtend` (in place of GCC's
    `crtbeginS.o`/`crtendS.o`), and the sanitizer runtimes (asan, tsan, msan, lsan, ubsan) in
    place of `librustc-nightly_rt.*`. These match the LLVM that generated the instrumentation,
    which the nightly's never did once blaze generates code with its own LLVM;
  - libc++, libc++abi and libunwind, static, per target.
- **`blaze-sysroot-<version>-<target>.tar.xz`, one per target, fetched when a build first targets
  it:** the C library side. Linux glibc: headers for every supported version, the Linux UAPI
  headers, `Scrt1.o`/`crti.o`/`crtn.o`/`libc_nonshared.a` built from glibc's source by our clang,
  and an abilist from which blaze makes the stub `libc.so.6`, `libm.so.6`, `ld-linux-*.so` for the
  glibc version a build asks for. wasm32-wasip1: wasi-libc built from a pinned commit by our
  clang. Windows GNU: mingw-w64's headers and import libraries, built as llvm-mingw builds them.
  macOS and Windows MSVC get no archive from blaze; their SDKs are the vendor's (below).

They land under `<BLAZE_HOME>/toolchains/cc-<sha256 prefix>/` and
`<BLAZE_HOME>/toolchains/sysroot-<target>-<sha256 prefix>/`. The Rust side keeps no download of
its own: `rustc --print sysroot` answers a directory blaze lays out itself, whose
`lib/rustlib/<triple>/lib` holds blaze-cc's runtimes, and std is only ever `library/`. rustup's
directory and static.rust-lang.org are never read.

**C for build scripts is blaze's.** Every build script runs with `CC`, `CXX`, `AR`, `RANLIB` and
their `_<triple>` and `HOST_`/`TARGET_` forms set by blaze to blaze-cc's tools, and with the
`CFLAGS`/`CXXFLAGS` cc-rs would want for the target (none: the config file carries them).
`CXXSTDLIB` is set to `c++`, so cc-rs tells rustc to link libc++, which blaze-link takes from
blaze-cc. A `CC` or `CFLAGS` in the user's environment is not passed to a build script; the
daemon says once, the first time it sees one, that it is not used and what opts a package out.
Since the tools are content-pinned by blaze-cc's sha256, they enter the code identity through the
toolchain's digest, and `unkeyed_env`'s gap closes: there is nothing left unkeyed to name.

**The opt-out, for a `-sys` crate that must build with another compiler** (a vendor compiler, a
GCC-only extension, the platform's libstdc++ ABI for a C++ interface it links against):

```toml
[workspace.metadata.blaze.c]
system-cc = ["openssl-sys", "my-gcc-only-sys"]   # by package name
```

Each named package's build script gets `CC`/`CXX`/`AR`/`CFLAGS`/`CXXFLAGS`/`CXXSTDLIB`/`SDKROOT` as
the environment has them. Those values, and each compiler they resolve to (its path, the sha256
of its binary and its `--version`), are inputs of the script's run key and of the code identity
of everything that links the package, so a changed host compiler is a rebuild, never a stale
reuse (rule 18). It sits in the root manifest beside `unsandboxed-build-scripts`, the one place a
registry dependency can be named. A compiler that is not reachable from inside the sandbox fails
naming it and `build-script-link-dirs`'s sibling for compilers.

## Hard parts, with prior art

What exists: [zig cc](https://codeberg.org/ziglang/zig) (glibc stubs from abilists, libc headers
for every target, crt from source, libSystem `.tbd`), [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
(2.7k stars: zig as cargo's linker, glibc version as a target suffix, `x86_64-unknown-linux-gnu.2.17`),
[hermetic_cc_toolchain](https://github.com/uber/hermetic_cc_toolchain) (zig cc as a Bazel
toolchain), [toolchains_llvm](https://github.com/bazel-contrib/toolchains_llvm) (LLVM release
archives plus a user-given sysroot), [hermetic-llvm](https://github.com/hermeticbuild/hermetic-llvm)
(a zero-sysroot Bazel toolchain that builds compiler-rt, libc++ and libunwind from source and
links glibc stubs from [glibc-stubs-generator](https://github.com/cerisier/glibc-stubs-generator),
an extract of zig's), [xclang](https://github.com/clice-io/xclang) (one PGO+ThinLTO clang, a
config file per target, glibc 2.17 sysroots, static libc++; fetches Apple's and Microsoft's SDKs
with license acceptance), [llvm-mingw](https://github.com/mstorsjo/llvm-mingw) (an LLVM-only,
redistributable Windows GNU toolchain), [xwin](https://github.com/Jake-Shadle/xwin) (fetches the
MSVC CRT and Windows SDK from Microsoft's manifest, pinnable by manifest and version, with
`--accept-license`) and [osxcross](https://github.com/tpoechtrager/osxcross). None is a toolchain
blaze can take whole (zig's is zig's driver and Zig's release cadence; the Bazel ones are Bazel
rules), but hermetic-llvm and xclang are the closest design and the pieces are theirs to borrow.

**Linux libc.** Options: a pinned sysroot (a Debian or CentOS 7 image's `/usr`, as toolchains_llvm
expects), or zig's way: one abilist of every glibc version's symbols, stubs generated per
version, one set of headers with version `#if`s, crt objects built from glibc's source. Decided:
zig's way, through hermetic-llvm's extract: a sysroot image is hundreds of MB per target and
fixes one version; stubs are a few hundred KB and let a build pick its floor. blaze generates the
stub shared objects itself (blaze-link writes ELF already) from the abilist in the sysroot
archive, so no stub file is shipped per version. The floor is glibc 2.17, rustc's own for its
Linux GNU targets. The default is 2.28 (2018: Debian 10, RHEL 8, every distribution still
supported in 2026), which every symbol std's Linux code links has; a workspace changes it with
`[workspace.metadata.blaze] glibc = "2.17"`, and the version is part of the target key. A symbol a
crate links that the chosen version lacks is a link error naming the symbol, the version that
has it and the setting. The ELF interpreter is a constant per architecture
(`/lib64/ld-linux-x86-64.so.2`, `/lib/ld-linux-aarch64.so.1`), never asked of a driver. musl,
static and self-contained, can follow as one more sysroot archive.

**macOS.** Apple's Xcode and Apple SDKs Agreement licenses the SDK only for use on Apple-branded
hardware and forbids altering or separately using it, so blaze cannot ship it; zig ships its own
libSystem `.tbd` and libc headers but cannot ship frameworks (zig #1349), and xclang has the user
fetch the SDK. Decided: the SDK stays the vendor's, and is keyed rather than pinned. blaze reads
the SDK of the active developer directory without running `xcrun` (the `xcode-select` link and
the SDK directory it points to), or `SDKROOT` when set, checks that its version is at least the
floor blaze tests (refusing an older one with the version it found and the one it needs), and puts
the digest of `SDKSettings.json` and of every `.tbd` a link reads, and the deployment target,
into the target key. An Xcode update is then a rebuild of what it changed, never a silent mix.
Pinning one exact SDK was rejected: Apple moves the Command Line Tools on its own schedule, and
every machine would stop at each update. On a Linux host (cross builds), the SDK is fetched from
Apple only with an explicit `blaze toolchain sdk macos --accept-license`, as xclang does. Whether
blaze may ship generated `.tbd` stubs of libSystem alone (zig does) is a legal question to settle
before anyone relies on it; nothing in this plan needs it.

**Windows.** The MSVC CRT and Windows SDK are not redistributable either. Decided: for the
`-msvc` targets, blaze fetches them as xwin does, pinned: the Visual Studio channel manifest's
sha256 and the CRT and SDK versions are in blaze (as `pins.txt` pins the nightly today), the
download needs `blaze toolchain msvc --accept-license` once (recorded in `BLAZE_HOME`), and
`$LIB`, `vswhere -latest` and the registry's newest kit are never read. For the `-gnullvm`
targets, the sysroot archive is mingw-w64 built by our clang, as llvm-mingw builds it, shipped
whole. MSVC's own `cl` is never run.

**The compiler that builds LLVM.** Two stages in `llvm.py`: the machine's compiler builds a
stage-1 clang, and stage 1 builds everything shipped (LLVM's libraries blaze links, blaze-cc,
the sysroots) against our libc++. What blaze runs then depends on the machine's compiler only
through stage 1's correctness. Until that lands, `blaze-llvm.json` records the host compiler's
`--version` (slice 3), so the dependency is at least named.

**blaze's own C and C++.** Once build scripts get blaze's `CC` (slice 8), blaze building blaze
(rule 2) builds the sandbox interposers and llvm-sys's wrappers with blaze-cc, and the C++ runtime
is blaze-cc's static libc++, not `$CXX -print-file-name=libstdc++.a` or `-lc++`. `brew --prefix`
goes; the pin already refuses any LLVM but ours.

## What is an error

Rule 3, case by case. Each message names the piece, the path or key it looked for, and the
command or setting that gets ours.

- blaze-cc missing or its sha256 wrong: the archive name, the expected sha256 and
  `blaze toolchain`. Never a clang, `cc` or `ld` on `PATH`.
- A target's sysroot archive missing: the target and `blaze toolchain target add <target>`.
- A glibc symbol newer than the chosen version: the symbol, the version that has it, the setting.
- No macOS SDK, or one below the floor: the path read, the version found, the floor.
- MSVC libraries not fetched, or the license not accepted: the command that does both.
- A sanitizer runtime missing for a target that has one: the file in blaze-cc. (Today's message
  points at the nightly's `rust-std`, until slice 5.)
- A blaze built without LLVM asked to assemble: says so (slice 1).
- A `system-cc` package whose compiler cannot be resolved: the variable, its value, the sandbox.

## Slices

Each lands alone, keeps the tree building, and leaves no fallback behind it.

1. **`asm!` only through the pinned LLVM.** `asm!`, `naked_asm!` and `global_asm!` go to the
   pinned LLVM's MC assembler in process and nowhere else. The C-compiler path (`Assembler::Cc`,
   the `PATH` walk for clang and `cc`, the `<stdin>:` error reading) is deleted; a blaze built
   without LLVM refuses with a typed error naming the block; `BLAZE_ASSEMBLER`, which picked the
   C compiler, is refused rather than ignored, and leaves `CHANGES_CODE`. The assembling tests run
   under `--features llvm`; one test pins the refusal without it. **Landed 2026-11-02 (owntc).**
2. **No rustup.** `Chosen::Rustup`, `rustup_home` and `installed` are deleted; `found` is blaze's
   own copy when `own::complete`, else `None`, and `ensure` downloads it, its error naming what is
   missing. rustup's directory is never read, not even as a copy of the same bytes, since a copy
   found by its directory's name is trusted by name. wasm32-wasip1's `rust-std` is pinned again in
   `pins.txt` (it was dropped on 2026-10-30, which left wasi-libc, which every proc macro's module
   links, to whatever rustup had installed). **Landed 2026-11-02 (owntc).**
3. **Build blaze-cc.** `llvm.py build --cc` also builds clang, lld, compiler-rt (builtins, crt,
   sanitizers) and libc++/libc++abi/libunwind for every supported target, archives them as
   blaze-cc, and `llvm.json` pins its sha256 per host. `blaze-llvm.json` records the host
   compiler's `--version`. CI only; no blaze code reads it yet.
4. **Fetch blaze-cc.** `blaze_cargo::toolchain::cc` fetches and checks it by sha256 as
   `own::fetch` does the nightly; `blaze toolchain` prints both. A missing or wrong archive is the
   error above.
5. **Sanitizer runtimes from blaze-cc.** `blaze-link` `runtimes`/`rpaths` and `unstable::runtimes`
   read compiler-rt's `libclang_rt.*` from blaze-cc; `librustc-nightly_rt.*` is no longer read.
6. **wasi-libc from source.** The wasm32-wasip1 sysroot archive is wasi-libc at a pinned commit,
   built by stage-1 clang; `blaze-link` `wasi` reads it, and the `rust-std` wasm32-wasip1 pin goes.
7. **No nightly download.** The sysroot `rustc --print sysroot` answers is a directory blaze lays
   out from blaze-cc; `--print target-libdir` and `cargo_version` come from blaze's pins, not from
   `multirust-channel-manifest.toml`. `own.rs`'s fetch and `pins.txt`'s components are deleted;
   `Chosen` and `ensure` are blaze-cc's. After this, blaze reads nothing the Rust project
   publishes but `library/`'s source, which is in blaze's own release.
8. **Build scripts' C is blaze's.** `blaze-build-script` `env.rs` sets `CC`/`CXX`/`AR`/`RANLIB`
   (and per-triple, `HOST_`/`TARGET_`) and `CXXSTDLIB=c++` to blaze-cc's tools for every script;
   the inherited values are dropped and said once; `[workspace.metadata.blaze.c] system-cc` is the
   keyed opt-out, and `blaze-codegen-code` `unkeyed_env` is deleted. Tested by real `-sys` crates'
   own suites (cc-rs's, openssl-sys vendored, zstd-sys, libgit2-sys) under blaze.
9. **Linux libc from blaze.** The Linux sysroot archives (glibc headers, abilist, crt, UAPI
   headers); blaze-link generates the glibc stubs for the key's version and takes crtbegin/crtend
   from compiler-rt; `c_driver`, `linker_dirs` and every `-print-file-name`/`-###` run are
   deleted; the interpreter is a constant; the glibc version joins the target key.
10. **macOS SDK keyed.** The SDK found without `xcrun`, its floor checked, its digest and the
    deployment target in the target key; `blaze toolchain sdk macos --accept-license` for Linux
    hosts.
11. **Windows pinned.** MSVC libraries fetched xwin-style at pinned versions with license
    acceptance; `$LIB`/`vswhere`/registry reads deleted; a `-gnullvm` sysroot archive from
    mingw-w64.
12. **blaze's own C/C++ by blaze-cc.** The interposers and llvm-sys's wrappers build with
    blaze-cc and link its static libc++; `brew --prefix`, `$CXX -print-file-name` and
    `-lstdc++`/`-lc++` are deleted from `vendor/llvm-sys/build.rs`.
13. **Two-stage LLVM.** `llvm.py` builds stage 1 with the machine's compiler and everything
    shipped with stage 1.

## Risks

- **Slice 8 changes behavior for every C dependency** that today compiles with the machine's
  GCC or Apple clang and links the system libstdc++. Most build unchanged under clang; a crate
  that relies on GCC extensions, or links a system C++ library's ABI (libstdc++'s `std::string`),
  needs `system-cc`. The test is real crates' suites, and the opt-out is keyed, so the cost is
  a line in a manifest, never a wrong build.
- **glibc default 2.28** may refuse a crate that links a newer symbol it found on the build host
  until the workspace raises the setting. The error names the setting.
- **Download size.** blaze-cc with every target's compiler-rt and libc++ is a few hundred MB
  unpacked (xclang's eight-target archives are the reference). Splitting per-target runtimes into
  the sysroot archives keeps a host's first download to its own targets.
- **The macOS SDK stays a machine input.** Keyed by digest, it can't make a build stale, but two
  machines with different Xcode versions build different bytes. That is Apple's license, not a
  choice; a project that needs identical bytes across machines pins Xcode.
