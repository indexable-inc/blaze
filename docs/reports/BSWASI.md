# Build scripts under WASI: the bench corpus

An experiment, off by default ([docs/cargo.md](../cargo.md#build-scripts-under-wasi-an-experiment-off-by-default)
says how it works and how to turn it on). Every build script of the bench corpus was built for
wasm32-wasip1 by blaze and run under Wasmtime beside its native run in the sandbox, and the two
were compared. This says which ran unchanged and why each other one did not, so that whether it
becomes the default can be decided on numbers.

## Result

624 runs of build scripts in 12 projects:

| category | runs | crates |
|---|---|---|
| works unchanged (same exit, stdout byte for byte, OUT_DIR files by path and bytes) | 473 | 312 |
| breaks on host cfg | 6 | 6 |
| breaks on threads | 0 | 0 |
| breaks on other | 135 | 109 |
| native run failed too (not counted against WASI) | 10 | 9 |

Of the 135 other, 64 differ from the native run only in what cc-rs prints about `CC`/`AR`/`CXX`,
which the run under WASI is not given (cargo does not document them for scripts), and in the
`cargo:rerun-if-env-changed=CC_KNOWN_WRAPPER_CUSTOM` lines cc-rs prints only when `CC` is set:
they exit 0 and leave the same OUT_DIR. Given those variables they would very likely count as
works, which would make 537 of 624 (86%); that was not measured.

No script broke on threads, and none on a `cfg` of its own: the 6 host-cfg breaks are all one
cause in a build dependency (below).

Per project (cargo's exit is 101 where a native script failed; `--keep-going` measured the rest):

| project | cargo | runs | works | host cfg | threads | other | native failed |
|---|---|---|---|---|---|---|---|
| axum | 0 | 55 | 46 | 0 | 0 | 9 | 0 |
| bat | 0 | 28 | 22 | 0 | 0 | 6 | 0 |
| bevy | 0 | 72 | 64 | 1 | 0 | 7 | 0 |
| biome | 101 | 62 | 45 | 1 | 0 | 8 | 8 |
| cargo | 101 | 41 | 28 | 0 | 0 | 12 | 1 |
| fd | 0 | 9 | 8 | 0 | 0 | 1 | 0 |
| helix | 101 | 29 | 22 | 0 | 0 | 7 | 0 |
| hyperfine | 0 | 20 | 13 | 0 | 0 | 7 | 0 |
| just | 0 | 18 | 15 | 0 | 0 | 3 | 0 |
| nushell | 0 | 90 | 66 | 4 | 0 | 20 | 0 |
| ripgrep | 0 | 10 | 8 | 0 | 0 | 2 | 0 |
| zed | 101 | 190 | 136 | 0 | 0 | 53 | 1 |
| total | | 624 | 473 | 6 | 0 | 135 | 10 |

Every run with its category and why is in
[tools/bench/results/2026-10-27-bswasi/](../../tools/bench/results/2026-10-27-bswasi/) (`README.md` lists
each crate by category, `runs.json` has one entry per run, `<project>.json` what was compared).

## Measured with

- Machine: ix VM `wasm32-bswasi`, arm64, 16 vCPU, 16 GiB, NixOS; the work directory on a
  10 GiB tmpfs (zed's cargo home on the root disk).
- Toolchain: rustup's nightly-2026-10-25 (aarch64-unknown-linux-gnu, with wasm32-wasip1), first
  on PATH inside `nix develop .#bench` (`.#bench-zed` for zed), `RUSTC` its absolute path.
- blaze: `cargo build --release -p blaze-engine --features wasi`, from these changes on main
  fdc1c53f15, before they were moved onto a409606643: zed with them as they land, the other 11
  projects before the guest was given CARGO_MANIFEST_DIR as a preopen of its own, which changes
  nothing where the package's path has no links in it, as theirs had not. Each project ran in a
  systemd unit, without `HOME`, natively and under WASI alike.
- Command, per project:

      cd harness && python3 bswasi.py record PROJECT --blaze /root/bsw/target/release/blaze-engine \
          --work /root/bsw/work --out /root/bsw/results --jobs 6        # zed: --jobs 3
      python3 bswasi.py report --out /root/bsw/results

  `record` runs `cargo check -Zunstable-options --compile-time-deps --workspace --all-targets
  --keep-going` through blaze's wrapper with an empty store, as harness/bscache.py does, and
  `BLAZE_BUILD_SCRIPTS=wasi-compare`, `BLAZE_SCRIPTS_BY_RUSTC=1`.
- The projects and revisions are harness/bench.py's. bun is left out, as harness/scripts.py
  leaves it out: its workspace needs its own C and C++ build first (bunprep.py).

## Why the others broke

Each crate is named with the projects it was found in.

### host cfg (6)

All six are tables that phf_codegen 0.11, a build dependency, lays out with rand 0.8's
`SmallRng` from a fixed seed. `SmallRng` is Xoshiro256++ where `target_pointer_width = "64"` and
Xoshiro128++ elsewhere (rand-0.8.6/src/rngs/small.rs), so under wasm32 the tables are another
layout: as valid, but not the same bytes. Each crate has phf_codegen ^0.11 (or chrono-tz-build,
which has it) as a build dependency on crates.io. phf_codegen 0.13 uses fastrand instead.

chrono-tz 0.9.0 (bevy), chrono-tz 0.10.0 (nushell), htmlize 1.0.6 (biome), markup5ever 0.12.1
(nushell), selectors 0.31.0 (nushell), web_atoms 0.1.3 (nushell).

A script whose own source tests the platform (`#[cfg(unix)]`, `cfg!(target_os = "linux")`,
`std::env::consts::OS`, ...) and broke for no other reason found: none. Of those that test
it, each broke first on one of the reasons below, mostly a program refused (zed's `build.rs`,
`cli`) or `CC` (psm). A first cut that counted any platform `cfg` in the source counted 37 here;
most were `#[cfg(target_env = "msvc")]` and `cfg!(windows)`, false natively and under WASI alike.

### other (135)

| why | runs | crates |
|---|---|---|
| `CC`/`AR`/`CXX` not given; only cc-rs's report of them and its `rerun-if-env-changed` lines differ, OUT_DIR the same | 64 | 52 |
| WASI has no permission bits: autocfg finds OUT_DIR "not a writable directory" | 31 | 16 |
| runs a program a script under WASI may not | 17 | 17 |
| blaze could not build it for wasm32-wasip1 | 13 | 13 |
| an operation WASI's std does not support | 4 | 4 |
| reads a file outside its package and OUT_DIR | 4 | 4 |
| asks its working directory, `/` under WASI | 1 | 1 |
| failed otherwise | 1 | 1 |

**`CC`/`AR`/`CXX` (64).** The environment under WASI is cargo's documented build-script
variables only, so cc-rs sees no `CC` and runs `cc`, which is the same compiler here. alloca,
blake3, cxx, libgit2-sys, libnghttp2-sys, libsqlite3-sys, libz-sys, link-cplusplus,
lmdb-master-sys, lz4-sys, onig_sys, openssl-sys, pcre2-sys, psm, ring, tree-house-bindings,
tree-sitter and 22 tree-sitter grammars (zed), wayland-backend, zstd-sys. Giving the script
cc-rs's variables (`CC`, `CXX`, `AR`, `CFLAGS`, ... and their target-suffixed forms) would
follow cargo's environment more closely than the documented list does.

**Permission bits (31).** wasi-libc's `stat` has no mode bits, so autocfg's check that OUT_DIR
is writable fails, and the scripts that use it unwrap the error or warn: num-traits 0.2.19 (10
projects), lock_api 0.4.12/0.4.13, num-bigint, num-complex, num-rational, rand 0.6.5,
rand_chacha 0.1.1, rand_pcg 0.1.2 (hyperfine), bigdecimal (nushell), eyre (bevy), fs-err (axum),
memoffset (zed), smartstring (helix); async-io and slab print a warning and leave out cfgs.
This is WASI's, not the runner's: only a change to autocfg (or to wasi-libc) helps.

**Programs refused (17).** git: cargo, cli and zed and remote_server (zed), cranelift-codegen
0.135.1 (zed), curl-sys, libssh2-sys, helix-loader, nu-cmd-lang (with cargo), ripgrep; sh:
tikv-jemalloc-sys (fd, biome); cargo: codspeed (biome); cmake: wasmtime-c-api-impl (zed);
drat-trim and rate: varisat (cargo).

**Not built (13).** blaze refused to build the script for wasm32-wasip1 and said why: a
function it cannot compile for wasm32 (aws-lc-sys 0.32.1, bat, biome_aria_metadata,
encoding_rs, livekit_api, octocrab, oem_cp, pbjson-types, proto, wasm-bindgen, wasmtime,
wasmtime-internal-fiber), or `env!("BUILD_TARGET")` in helix_loader, which helix-term's script
depends on, unset in the wasm build. These are the wasm32 backend's, not WASI's.

**Unsupported (4).** `std::env::join_paths`: system-deps prints `cargo:include=` only when it
succeeds (system-deps-7.0.7/src/lib.rs:483), so gio-sys, glib-sys and gobject-sys (zed) leave
it out. html5ever 0.27.0 (nushell): `Unsupported` at its build.rs:36.

**Outside its package (4).** edit_prediction_cli and eval_cli read `crates/zed/Cargo.toml` and
extension_host reads `../extension_api/wit` (zed), sibling packages of the workspace; uucore
(nushell). The preopens are the package's directory and OUT_DIR only.

**Working directory (1).** aws-lc-sys 0.45.0 (zed) joins `std::env::current_dir()` with its own
files; under WASI that is `/` (wasi-libc's own working directory), so it hands cc
`/aws-lc/...`. Starting the guest in the package's directory needs a `chdir` before `main`,
which the std overlay does not do.

**Failed (1).** rustix 1.1.2 (just): its script failed, saying "not supported by WASI yet".

### native failed (10)

biome's seven `*_analyze` crates write into their source tree, which the native sandbox
refuses (as on main); cargo's build-rs-test-lib; zed's zstd-sys. Not counted against WASI.

## Reproducible, and host-independent

The module is the same bytes from two cold builds (empty store and target directory) of the
same package: sha256 60514e6df2291ff35e0ec1e9400b633ea85dd1cd371a36294df4123d65a12b2f, the
e2e package with a C file, a call of rustc and a refused git. A wasm32-wasip1 module names no
host, so one built on aarch64 Linux is the one an x86-64 host would build; that was not
measured here (one VM, arm64).

The run's key (bscache's Nix model: the module's bytes, the package's files, the scrubbed
environment and arguments, the toolchain, each allowed tool as PATH finds it) has no host in
it but what the tools are. The store's own keys still mix in the blaze build and the
toolchain's identity, whose `lib/` names are the host triple's, so a run kept on one kind of
host is not found on another yet.

## Found on main

In `nix develop .#bench` cargo gives build scripts `RUSTC=rustc`, a name, and the rustc blaze
answers scripts with (script_sandbox's `with_own_rustc`, as_rustc.rs) refuses it: "blaze can't
answer `rustc --version` for a build script: `BLAZE_SCRIPT_RUSTC` does not name the
toolchain's rustc". rustversion and every script that asks `rustc --version` fail natively,
with or without `BLAZE_ONLY_ASKS`. bswasi.py sets `RUSTC` to the absolute path of `rustc` on
PATH. The Nix toolchain of `.#bench` also has no wasm32-wasip1, so the wasm builds need
rustup's.

## What is left

- Give scripts under WASI cc-rs's variables, and measure again (64 runs).
- Start the guest in the package's directory (1 run, and any script that asks `current_dir`).
- Preopen the workspace root read-only, or say that a script reading a sibling package is
  refused (3 runs in zed).
- A second host type, for the module's bytes and for the store's keys.
- Whether this becomes the default is Andrew's.
