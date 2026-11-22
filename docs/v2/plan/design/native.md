---
status: plan
summary: Native code in a blaze program (C, C++ and assembly from -sys crates); an inventory of tempo's lock, native objects as records made behind an abstract ToolRun effect, and blaze's own in-process loader that places one native image from a pure load plan (crates/blaze-native-load), binding TLS, unwind, initializers and system libraries.
date: 2026-11-18
---
# Native code: records, one image, our own loader

This extends, and does not replace, the native pipeline of
`blaze-int-1112-notes/port27/tempo-run/NATIVE.md` (W1..W11) and run.md's "native image as a
task". What exists in the tree today:

| Piece | Where |
| --- | --- |
| Archive, Mach-O and ELF readers into one object model (`RelocKind` normalized) | `crates/blaze-native/src/{ar,macho,elf,object}.rs` |
| Symbol resolution with lld's rules (archive members pulled on demand, weak vs strong, common, duplicates refused) | `crates/blaze-native/src/resolve.rs` |
| Layout: regions, GOT, stubs, veneers, every pc-relative relocation applied, the rest left as fixups | `crates/blaze-native/src/layout.rs` (`lay`) into `image.rs` (`Image`) |
| Unwind rows, the BNL2 descriptor, install names | `crates/blaze-native/src/{unwind,descriptor,libraries}.rs` |
| The image written as a dylib or .so | `crates/blaze-export/src/native_{dylib,so,merge}.rs` |
| `dlopen` of that file in the launcher | `crates/blaze-launch-rt/src/native_lib.rs` |
| **The load plan and its binding (this doc's new piece)** | `crates/blaze-native-load/` |

## Inventory: tempo's Cargo.lock

Read from the copy at `port27/tempo-deps/copy/tempo/Cargo.lock` and the unpacked registry
sources; nothing was run. Totals:

- 1348 packages in the lock; 128 from git, 1220 from the registry.
- 249 registry packages are not unpacked on this Mac; of those, only `keccak-asm 0.1.9` and
  `tracy-client-sys 0.29.0` look native by name (unread, listed below as such).
- 137 unpacked registry packages have a `build.rs` or a `links` key.
- On the path of `tempo`'s main: a walk of the lock's edges from `tempo` reaches 1299 of 1348.
  The lock merges dev, target and feature edges, so this over-counts; every native crate below
  is reached except `libfuzzer-sys`. The feature-aware set (NATIVE.md section 2) is the one
  marked "main" below.

### Crates that compile native code (25)

| Crate | Builds | Needs at run time | main |
| --- | --- | --- | --- |
| aws-lc-sys | cc + cmake + nasm (bindgen optional); ~256 .c, 134 .S | libcrypto objects; `CRYPTO_*` thread-locals; static ctors none on arm64 | yes |
| ring | cc (+ nasm on x86) over C and perlasm-made .S | its own objects; `OPENSSL_armcap_P` (data, initialized from Rust) | yes |
| blst | cc, one `server.c` + assembly | objects only | yes |
| c-kzg | cc over `ckzg.c` (needs blst's headers) | objects only | yes |
| secp256k1-sys 0.10.1 / 0.11.0 / 0.14.1 | cc over the vendored libsecp256k1, each version's symbols prefixed | objects only; three copies must not collide (they are prefixed) | yes |
| blake3 | cc over the NEON .c | objects only | yes |
| librocksdb-sys | cc, 329 C++ sources (`rocksdb_lib_sources.txt`) | libc++ (system dylib), C++ static ctors, C++ exceptions inside rocksdb, thread-locals | yes |
| reth-mdbx-sys (git) | cc over `mdbx.c` | pthread, the file system; thread-locals | yes |
| zstd-sys | cc over the zstd sources | objects only | yes |
| lz4-sys | cc over lz4 | objects only | yes |
| bzip2-sys | cc, 6 .c | objects only | via deps |
| libz-sys | cc over zlib, or the system `z` | objects or `libz.dylib` | via deps |
| tikv-jemalloc-sys | configure + make | objects; thread-locals (ELF: initial-exec unless `--disable-initial-exec-tls`, build.rs:293); a malloc zone on macOS | yes |
| sha3-asm | perlasm to .S, then cc | objects only | yes |
| keccak-asm 0.1.9 | (unread: not unpacked) | presumably as sha3-asm | ? |
| libusb1-sys | cc over libusb | IOKit, CoreFoundation frameworks | yes |
| hidapi-rusb | cc over hidapi's libusb backend | libusb1-sys's objects | yes |
| signal-hook | cc over `extract.c`, only with `extended-siginfo-raw` | objects only | yes (feature) |
| alloca | cc over `alloca.c` | objects only | via deps |
| antithesis-instrumentation | cc | objects only (no-op off Antithesis) | via deps |
| tracy-client-sys | (unread: 0.29.0 not unpacked) | C++ runtime, a thread, thread-locals | ? |
| llvm-sys | links the system LLVM | `libLLVM` (dylib) | no (blaze's own deps only) |
| generator | assembly only for ppc64le | none on arm64 | via deps |
| findshlibs | a cc `expand` probe only | none | via deps |
| libfuzzer-sys | cc, C++ | (not reached from main) | no |

Also cfg-dependent and not built for arm64 macOS or Linux: `iana-time-zone-haiku` (C++ on
Haiku), `symbolic-demangle` (C++ only with `swift`).

### System libraries only (no sources compiled)

- `clang-sys` (`links = "clang"`: libclang, a dylib)
- `inotify-sys` (Linux libc)
- `system-configuration-sys` (the SystemConfiguration framework)
- Windows only: `ipconfig`, `ntapi`, `winapi`
- `wit-bindgen` (wasm only)
- Rust-side `#[link]`s (NATIVE.md): the frameworks CoreFoundation, CoreServices, Foundation,
  IOKit, OpenDirectory, Security and ServiceManagement; `objc`; libc `c`

### Bindgen or Rust generation only (no native code)

`libproc`, `io-uring`, `bindgen`, `wasm-bindgen-shared`.

### Probes and cfg only (the rest of the 137)

`anyhow`, `crc32fast`, `eyre` (autocfg), `getrandom`, `httparse`, `libc`, `linkme-impl`,
`object`, `paste`, `portable-atomic-util`, `proc-macro2`, `quote`, `ref-cast`, `rustix`,
`rustversion`, `serde`, `serde_core`, `syn`, `thiserror` 1 and 2, `typeid`, `zerocopy`, `zmij`,
`defmt`, `aws-lc-rs` (`links` + cfg), `rayon-core` (`links` only), and the rest of the 137,
which only print `cargo:rustc-cfg` or `rerun-if` lines. These are build-script work for D65, not
native code.

What the run-time column asks of a loader, summed: plain objects (all), system dylibs and
frameworks by install name (libc++, IOKit, CoreFoundation, libz), thread-locals (aws-lc,
rocksdb, mdbx, jemalloc, tracy), C++ static constructors and exceptions inside C++ (rocksdb,
tracy), and a malloc that replaces the system's (jemalloc).

## Decisions

1. **One native image per program, from blaze-native.** The readers, resolver and layout that
   exist stay the one home (rule 30); nothing here parses or relocates a second time.
2. **A native object is a record, made behind an abstract `ToolRun` effect.**
3. **blaze loads the image itself, in process, from a pure load plan;** dyld and ld.so are asked
   only for system libraries, by install name and symbol name.
4. **Thread-locals go through the launcher:** a Mach-O `tlv_get_addr` thunk and key, an ELF
   TLSDESC resolver; ELF initial-exec TLS is refused by name.
5. **Unwinding is registered before any image code runs,** and a panic or C++ exception crosses
   the boundary only through `extern "C-unwind"`.

### 2. Native objects are records; `ToolRun` makes them

A native object record is keyed, before the work, from:

- the source text of each translation unit and every header it includes (the header list is the
  run's read list, checked as any read list is);
- the flags (defines, include roots, `-O`, `-f` options, the language standard), in order;
- the tool's identity: the digest of the compiler's own records (its binary or its wasm module
  and its resource headers), never its path;
- the target (`aarch64-apple-macosx<min>` or `aarch64-unknown-linux-gnu`).

The value is one object (Mach-O or ELF bytes) per translation unit, so one edited `.c` file
rebuilds one object (rule 65). An archive is never a record: the resolver reads members, and a
crate's native output is its list of object records.

The effect is abstract: a core asks for `ToolRun { tool, argv, inputs, outputs }` and is answered
with output bytes or a typed refusal. Three executors can answer it:

| Executor | Runs | Cost | Cannot do |
| --- | --- | --- | --- |
| hvbox (hvbox.md, D92) | the real clang, ld, make, cmake as macOS guests with every syscall answered | near native | nothing on the owner's Mac until he says (it panicked the kernel); not built for Linux |
| clang compiled to wasm, run in Wasmtime | the C, C++ and assembly front ends and the arm64 back end | about 2x to 4x a native clang per translation unit (Cranelift-compiled module, no LTO of the host); one load of a ~60 MB module per helper | no `configure` and `make` (jemalloc), no perl (ring and sha3-asm's perlasm: their published .S is used instead), no nasm (x86 only), no cmake (aws-lc's cmake is replaced by its source list, as NATIVE.md W3 says) |
| a published record | nothing: the record is fetched by key from the shared cache, filled by CI | a fetch | anything not on the published list; a private patch to a -sys crate builds locally |

The published list starts with every native crate of the inventory's table (they are popular and
their keys are stable), so a cold tempo build compiles no C at all.

### 3. The loader

The image is laid by `blaze_native::layout::lay` as today. Then, instead of writing a dylib
and calling `dlopen`:

- `blaze_native_load::plan::plan(&image)` makes a `LoadPlan`: segments with protections (RX for
  code; R for the GOT after binding, a RELRO; RW for data and zero-fill), the bytes to copy,
  one patch per 64-bit cell, the import list, the initializers in link order, the TLS template,
  the unwind ranges, the exports. The plan holds no address of the process: it is a record,
  keyed by the image, and its bytes are deterministic (`LoadPlan::encode`).
- `blaze_native_load::bind::bind(&plan, &site)` answers every store at a base, given what the
  driver found: each import's address, the Rust words' entry thunks and the TLS helpers.
- The driver (the launcher; not written) runs the eight steps bind.rs lists.

Why not `dlopen` of a written file:

- One loader for macOS and Linux, tested in process without a file system, and the same plan
  places the image in a test, in the launcher, or in a remote worker.
- No file to write, sign or clean up per program (dyld refuses an unsigned page as executable;
  writing and ad hoc signing a dylib per edit is a step on the edit loop).
- The image is Rust's partner: its imports of Rust words are bound to the slots' entry
  thunks directly, not through a second symbol table.

What it costs: the code is a private copy (dirty pages) instead of a file mapping shared by every
process that loads it, about 15 MB for tempo's natives (NATIVE.md's size), and on macOS the code
must be `MAP_JIT` memory written under `pthread_jit_write_protect_np`, as the launcher's code
area already is.

#### Symbols, both ways

- **Native to Rust:** an undefined name resolved to a Rust function (`Resolution::Rust`) is a
  word: its cell and every pointer row (`FixTo::Word`) are bound to that slot's entry thunk, so
  a tier-up moves no native code.
- **Rust to native:** the image's exports (sorted, `bind::export`) are published to the code
  records that call C, before step 8.
- **System libraries:** the image's `libraries` are opened by install name with `dlopen`
  (`RTLD_LAZY | RTLD_LOCAL`), and each import is looked up flat over them in load order, then in
  the launcher itself. The lookup is eager (all of them, before anything is placed): a strong
  import that no library defines is `BindError::Unresolved` naming it, never a crash at the
  first call. Lazy binding is dropped: there are about two thousand imports and the lookup is
  microseconds each.
- **Weak:** a weak definition loses to a strong one at resolve time (blaze-native's resolver);
  a weak reference that no library defines binds to 0. A common symbol becomes zero-fill of its
  largest size and alignment, laid by `lay`.

#### Thread-locals

- **Mach-O:** each `__thread_vars` descriptor is `(thunk, key, offset)`. `lay` writes the
  descriptor's first cell as an import of `__tlv_bootstrap`; the plan turns it into the
  launcher's `tlv_get_addr` thunk and adds the key cell at +8. The thunk keeps every register
  but x0, as dyld's does (the compiler relies on it), and on a thread's first access allocates
  the image's block from the template (`__thread_data` then `__thread_bss`). `__tlv_bootstrap`
  is never looked up.
- **ELF:** a TLSDESC pair is the launcher's resolver and the variable's offset in the block.
  An initial-exec access (a fixed offset from `tpidr_el0`, `TpRel`) needs the C library's static
  TLS area and is refused (`PlanError::InitialExecTls`). The one crate in the inventory that
  emits it is jemalloc, whose build is given `--disable-initial-exec-tls` (its own option;
  tikv-jemalloc-sys build.rs:293 already passes it under a feature).

#### Initializers

C++ static constructors (`__mod_init_func`, `.init_array`) run in link order on the program's
first thread, after binding and unwind registration and before the entry. Terminators
(`__mod_term_func`, `.fini_array`) and `atexit`/`__cxa_atexit` registrations run at the
program's exit, after Rust's.

#### Unwinding and exceptions

- **macOS:** compact unwind rows (`__unwind_info`) cannot be given to `__register_frame`, so
  the driver registers one callback with `__unw_add_find_dynamic_unwind_sections` (a libunwind
  entry point present since macOS 14; rule 45, a private API, its symbol tested), answering
  `dso_base`, `__unwind_info` and `__eh_frame` for the image's text range.
- **Linux:** `__register_frame` with the whole `.eh_frame`.
- **Crossing:** a C++ exception thrown and caught inside C++ (rocksdb) needs only the
  registration above. A Rust panic that unwinds through C frames, or a C++ exception through
  Rust frames, is defined only where the Rust side is `extern "C-unwind"`; with `extern "C"`
  the unwinder stops at the boundary and the process aborts, as Rust's reference says. blaze
  keeps that rule and does not widen it.

#### Malloc

jemalloc, when the program asks for it, is a native object like any other; its macOS zone
registration runs as one of its initializers.

### Floors and ratios

- Plan: one pass over the image's fixups, plus a sort; under 1 ms for tempo's image (about
  50 000 cells). Floor: the sort. Ratio: not measured.
- Bind: one store per cell, plus one lookup per import (about 2 000 `dlsym`s, a few ms).
  Floor: the copy of the bytes (15 MB at memory bandwidth, about 2 ms). Ratio: not measured.
- Against `dlopen` of a written file: the floor is the same copy (dyld's page faults do it
  lazily, which is cheaper when most of the code is never run); ours writes it eagerly.

## The brief's items and where they live

| Item | Home |
| --- | --- |
| Parse with bounds checks | `blaze_native::{macho, elf, ar}` (existing); planted lies in `blaze-native-load/tests/macho_lies.rs` |
| Lay out | `blaze_native::layout::lay` (existing) |
| Resolve | `blaze_native::resolve::plan` (existing) |
| Relocate | `blaze_native::layout` writes (existing, private); `blaze_native_load::a64` reads back |
| The plan | `blaze_native_load::plan` |
| The binding | `blaze_native_load::bind` |
| The driver (map, protect, TLS, unwind, inits) | the launcher: not written |
| The `ToolRun` effect and object records | not written |
