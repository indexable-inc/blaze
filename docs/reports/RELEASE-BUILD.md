# How the release binaries are built

Branch `build-profile`, from main at 40ddac92ab, 2026-10-21. The question was whether blaze is
built the fastest way it can be for the daemon's edit loop and its cold start: the allocator,
the Cargo profile, and profile-guided optimization (PGO). What a release ships now: the `dist`
profile (`release` with thin LTO), optimized with a profile of its own work, built by
`.github/scripts/build_release.py`. Against the plain `release` build it replaces, on Linux:
the cold start 14% shorter on nushell and on ripgrep, an edit of a function 8% shorter on
nushell, the same peak memory.

## How it was measured

`harness/abbench.py` (new) runs builds of blaze against each other on `bench.py`'s edits of a
real project, in rounds whose order rotates, each build with its own copy of the project, home
and store; a turn starts a daemon from nothing (the cold start: cargo's first build and the
wait until the daemon is ready), makes every edit three times and takes it back. Every number
is a median over rounds x 3 samples, against the first build of the table; a bracket is the
bootstrap 95% interval of the change, given where it matters. The first round's cold start
also checks every dependency with cargo and is left out of the cold-start medians.

- Linux: dev-aws-5, c8i.16xlarge (32 cores, 64 threads, 480 MiB L3), THP `madvise`. Every
  experiment pinned to cores 0-15 (`taskset`), builds to the other half or run before.
  ripgrep 15.1.0 and nushell, Cranelift.
- macOS: an M1 Max shared with other agents' builds (load 30 to 48), so wall times there are
  noise; the daemon's workers' CPU cycles (`proc_pid_rusage`) and lifetime peak footprint are
  what is reported from it.

"Edit work" is the daemon's `edit-to-binary` less the link: what the daemon computes for an
edit of nushell, about 220 ms of a 270 ms edit. On ripgrep an edit is mostly the link and
`cargo`, and the profile does not move it outside the noise.

## Where an edit's time goes

`perf record` of the workers during nushell edits (mimalloc v3, release): salsa revalidating
memos (`maybe_changed_after`, `verify_memo`, `try_claim`) is 73% of user cycles; the allocator
1.8%, `memcpy`/`memmove` 0.5%, drops 0.8%. So no allocator can make an edit more than about 2%
faster, and none did.

## Allocator

Linux, 4 rounds, against mimalloc v3 (what the `mimalloc` 0.1.52 in `Cargo.lock` builds by
default):

| allocator | nushell edit work | nushell cold start | nushell peak | ripgrep cold start | ripgrep peak |
|---|---|---|---|---|---|
| mimalloc v3 | 229 ms | 25.9 s | 15.4 GB | 4.96 s | 2.58 GB |
| mimalloc v2 | +2% | +7% | -1% | +5% | -12% |
| mimalloc v3, large OS pages | -3% | +3% | +1% | 0% | +1% |
| jemalloc (tikv-jemallocator) | +1% | +7% | -10% | +5% | -21% |
| jemalloc, background threads | -2% | +8% | -10% | +5% | -18% |
| snmalloc | +3% | +12% | +28% | +6% | +19% |

On the Mac: peak memory mimalloc v2 -17% (nushell) and -16% (ripgrep), jemalloc -8% and -7%,
snmalloc +18% and +30%; nushell edit cycles v2 +6%, jemalloc -1%.

Kept: mimalloc v3, the fastest cold start; no allocator changes an edit outside the noise.
jemalloc holds 10-21% less memory for a 5-8% longer cold start, which is the trade to make
if memory ever matters more than the cold start. snmalloc is worse at everything.

## Cargo profile and PGO

The final run (E), from main at 40ddac92ab with the worker fix below, 5 rounds. `dist` is
`release` with `lto = "thin"`; the PGO builds are `build_release.py`'s, the instrumented build
and the optimized one of the same profile:

| build | nushell edit of a function | nushell edit work | nushell cold start | ripgrep cold start | ripgrep first edit | peak |
|---|---|---|---|---|---|---|
| release | 267 ms | 225 ms | 25.4 s | 5.01 s | 1600 ms | 15.7 GB, 2.6 GB |
| dist (thin LTO) | -5% | -4% [-9, 0] | -5% | -3% | -9% | +1%, -1% |
| **dist + PGO** | **-8%** [-14, -2] | **-5%** [-10, -3] | **-14%** | **-14%** | **-16%** [-20, -2] | 0%, +1% |
| release + PGO | -7% | -2% | -12% | -13% | -10% | 0%, 0% |

Every one of dist + PGO's cold starts was shorter than every one of release's: nushell 21.8 to
22.3 s against 25.0 to 26.9 s, ripgrep 4.3 s against 4.9 to 5.2 s.

The runs before it, from main at 1f28e9877b, on nushell's edit work and cold start:

| build | B (3 rounds) | C (5 rounds) | D (5 rounds, quiet host) |
|---|---|---|---|
| thin LTO, 16 codegen units | -7%, -4% | -6%, -6% | -5%, -4% |
| thin LTO, 1 codegen unit | -7%, -8% | -5%, 0% | |
| fat LTO, 1 codegen unit | -5%, -8% | | |
| `-Ctarget-cpu=x86-64-v3` | -1%, -3% | | |
| PGO, release | -3%, -14% | | 0%, -11% |
| PGO from the release-profile instrumented build, on thin LTO, 1 codegen unit | -9%, -12% | -3%, -5% | |
| the same, 16 codegen units | | -5%, -4% | -3%, -3% |

In these runs the profile came from an instrumented `release` build whatever profile it was
used on, and on thin LTO it gave most of both wins once (B) and little of either twice (C, D;
C ran beside builds on the other cores). E's builds take the profile from an instrumented
build of the profile they optimize, as `build_release.py` does, and got both. On the Mac (B
and C's builds, cycles), thin LTO took 4-7% fewer cycles to cold-start nushell and PGO 6%
(the one interval clear of zero); edit cycles moved within the noise (-3% to +6%).

Build time of blaze (`cargo build -p blaze-engine`, Linux, 32 cores):

| profile | from clean | after a one-line change in `crates/blaze-engine` |
|---|---|---|
| release | 73 s | 18 s |
| thin LTO, 16 codegen units (`dist`) | 83 s | 46 s |
| thin LTO, 1 codegen unit | 241 s | 118 s |
| fat LTO, 1 codegen unit | 427 s | 307 s |
| `build_release.py` (dist twice, training between) | 319 s | |

Adopted: thin LTO with the default codegen units, as the `dist` profile, with PGO. One
codegen unit and fat LTO cost three to six times the build for nothing thin LTO did not
already give. `release` stays as it is: the harnesses, CI and every agent build it again
after each change, and thin LTO makes that rebuild 2.5 times longer for 4-5% of edit work.

## PGO, as a release is built

`build_release.py --target T` builds `dist` instrumented (`-Cprofile-generate`), runs it on
fd, bat, just and hyperfine (projects the benchmarks do not measure) through
`abbench.py --train` (a cold start and every kind of edit, twice), merges the counts with the
toolchain's `llvm-profdata` (`llvm-tools`), refuses a profile under 10^9 counts, and builds
`dist` again with `-Cprofile-use`. The release workflow runs it in place of its `cargo build`,
on each target's own runner (all three are native), so there is one way release binaries
are made. It costs about four times the plain build: 319 s against 72 s on the dev host, an
estimated 25 to 30 minutes on a hosted runner, which is why the job's limit is now 120
minutes.

What it took:

- A killed process writes no counters. `blaze stop` kills the workers, so
  `--train` ends a turn by killing the daemon, after which each worker sees its input close
  and exits by itself; each process writes its own file (`LLVM_PROFILE_FILE=%p`), and the
  merge waits until no instrumented process is left. With one shared file, a spare still
  writing it as the merge read it crashed `llvm-profdata`.
- On main, a worker that loaded its workspace beside cargo's first build never ended when
  its daemon died (below), so the training waited 120 s per project for nothing and got a
  quarter of the counts: 13 against 57 billion.
- On macOS the `cc` crate passes `-Cprofile-generate` on to Apple clang, whose profile format
  is not rustc's LLVM's, and the instrumented blaze crashed at start
  (rust-lang/rust#163640): the C code (mimalloc, ring) is built with
  `-fno-profile-generate`/`-fno-profile-use`. GCC on Linux accepts the same flags.

Superseded on 2026-10-21 by the next section: the training moved to its own workflow, and the
release workflow builds once, with a pinned profile.

## One build from every way in

Branch `one-build`, from main at 22b1267233, 2026-10-21. Until then the flake and
`cargo install` built `release`, without thin LTO or PGO. Now:

| way in | builds |
|---|---|
| a release's binaries | `build_release.py --target T --strict`: `dist` with the profile `tools/pgo/profiles.json` pins for T |
| `nix run`, `nix profile install`, `nix build .#daemon` (and `.#dist`, `.#blaze`, the demo) | the same script inside the derivation, the same profile fetched by its sha256 |
| from a checkout | the same script |
| `cargo install --locked --profile dist blaze` | `dist` without PGO |

**One profile for every build.** A build has to be reproducible from the commit alone, so the
profile can no longer be trained inside the release job: the `pgo` workflow trains it, one
per target on the runner that target's release is built on, uploads it to the `pgo-profiles`
release under a name that carries its sha256, and commits that name and hash to
`tools/pgo/profiles.json`. `build_release.py` downloads the pinned file and refuses any other; the
flake fetches it with `fetchurl` and the same hash (a nix build has no network) and passes it
to the same script, which checks it again.

**Why not commit the profiles.** One is 56 MB on macOS and 67 MB on Linux (5 MB with zstd
-19, 13 MB with gzip), and a retrain replaces all three: in the repository that is 15 to 200 MB
of history per retrain, kept forever. As release assets they cost the repository a few lines
of JSON.

**A profile finds a function by its symbol.** Read on Linux x86-64 with `nm` and
`llvm-profdata show --all-functions`: every symbol of a Rust crate carries the crate's hash
(`Cscx9Nkki9Or4_6blaze`), and cargo derives that hash from the whole graph of the build:
each crate's name, version, source and features, the profile (incremental or not included),
the toolchain, and a wrapper of rustc around the workspace's crates (not RUSTFLAGS, and not
the absolute path: the runner's build and the dev host's have the same hashes). A build whose graph differs finds next to
nothing in the profile, and LLVM says nothing about a function it cannot find. Found by
comparing the binaries' symbols, crate by crate:

- The first nix build matched the profile in the 211 registry crates and in none of the 27
  of the workspace and the vendored rust-analyzer. nixpkgs builds through cargo-auditable,
  which cargo runs as `RUSTC_WORKSPACE_WRAPPER`, and cargo hashes that wrapper into every
  local crate (read from `cargo build -v` in and out of the sandbox: the command lines differ
  in the wrapper, the linker and the `-C metadata` values; the unit graphs are the same). The
  flake's `dist` now sets `auditable = false`, and the script clears `RUSTC_WRAPPER` and
  `RUSTC_WORKSPACE_WRAPPER`, which `blaze enable` would otherwise have set for anyone
  building from a checkout.
- The dev host's shell sets `CARGO_INCREMENTAL=1`, which turns incremental on for the
  workspace's crates and changes their hashes: run from that shell, the build at 9c15d7035b
  (the first pin the workflow committed to main) was refused by `--strict`, and passed with
  the variable unset. The script sets `CARGO_INCREMENTAL=0`, `dist`'s own setting.
- `cargo install` from a checkout gets other hashes again, with or without `--target`, and
  one from crates.io builds other packages altogether (`blaze-hir` for `hir`, the registry
  for the path), so no profile trained from the repository applies to it. Handing it the
  profile (`--config build.rustflags=['-Cprofile-use=…']`, which it does pass to rustc) builds
  a blaze whose workspace crates have hashes the profile does not have, and LLVM warns of
  nothing.
- Any change to Cargo.lock, a manifest or the toolchain changes the hash of every crate above
  it, the workspace's included.

So each pin records the hash its profile has for blaze (`crate`), the script reads the
hash of the blaze it built from the binary's symbols and warns when they differ, a release
(`--strict`) refuses to be built that way, and the `pgo` workflow runs by itself on a push to
main that touches Cargo.lock, a manifest or `harness/toolchain`.

**Older code of the same crates.** LLVM matches a function by symbol and by a hash of its
control flow, uses the counts where both match and leaves the rest as if there were no
profile, warning "function control flow change detected (hash mismatch)". `build_release.py`
counts those warnings (rustc gives them for the crates cargo does not cap, the workspace's and
the vendored rust-analyzer's, also when cargo replays them for a crate already built) and ends
with the number. On Linux x86-64 with the profile trained at 22b1267233: 0 at that commit, 24
after a one-line edit to `main.rs`, 1,622 at d906695d51, 23 commits and 6,700 lines older; the
profile has about 187,000 functions.

**`cargo install` and PGO.** Cargo reads profiles from the root manifest only, and an
installed crate is its own root, so the published `blaze` manifest now carries `release` and
`dist` (`publish_crates.py`) and `--profile dist` works from crates.io. Compiler flags are
another matter: `[profile.*] rustflags` is the unstable `profile-rustflags` feature (stable
cargo 1.99.0 refuses the manifest: "requires a nightly version of Cargo"), a build script's
`cargo::rustc-flags` accepts only `-l` and `-L`, and a crate's own `.cargo/config.toml` is not
read when it is installed. The person installing could pass the flag, but not a profile that
fits (above). So the install is `dist` without PGO, which on Linux (run E above) starts 10%
slower and edits 3% slower than the release's binaries, and the README says so.

**`release` stays as it is.** The harnesses, the gate and CI build `release` after every
change, and `cargo install --profile dist` reaches the shipped profile without making
`release` slower to rebuild (18 s against 46 s after a one-line change), so nothing that
works on blaze pays for thin LTO, and nothing that ships goes without it.

**Not byte for byte.** The recipe, the Rust toolchain, the crates and the profile are the
same; the C compiler and the linker are not (Ubuntu 22.04's on the runner, nixpkgs' in the
flake), so the C code (mimalloc, ring) and the layout differ.

**Found on the way.** The release workflow of the previous section installed the toolchain
without `rust-src`, so the training daemon would have refused every project and the first
tagged release would have failed; the `pgo` workflow installs it. The README's `nix profile
install github:indexable-inc/blaze#blaze` named a package the flake did not have;
`packages.blaze` is now the daemon. And the archive and the flake's `dist` copied three named
binaries, missing `blaze-rustc-first`, which `blaze` runs in a workspace's first build;
`build_release.py --out` copies every binary cargo reports for blaze, so neither keeps a list.

**Verified** (2026-10-21, `RUST_BACKTRACE` unset). The profiles pinned were trained by the
`pgo` workflow (run 39640967658, from a throwaway branch: a dispatch needs the workflow on
main) at code identical to 22b1267233. Linux x86-64 (dev-aws-5): `build_release.py --strict`
downloads the pinned profile, checks it and builds; `nix build .#dist .#daemon` builds the
same in 82 s, with the same crate hashes and the same number of symbols of blaze (11,236),
of `hir_ty` (25,845) and of `serde_json` (1,357), and 7 KB less code; both have all four
binaries. Under the nix-built blaze, `difftest.py corpus --layouts --mixed 8` matches;
`cargo test --release -p blaze-engine` passes (117). With `RUSTC_WORKSPACE_WRAPPER` set the script
builds the same crates. macOS AArch64: `build_release.py --strict` builds (392 s on the loaded
Mac) and the corpus with `--mixed 8` matches under its binary; `nix build .#dist .#daemon`
builds with the same crate hashes and all four binaries (1,208 s). `cargo install
--profile dist --path crates/blaze-engine` builds and installs all four binaries (Linux). Not run:
the release and pgo workflows on main (the pin job's push to main is untested), Linux AArch64
beyond the training, the LLVM backend.

## Rejected

- **`panic = "abort"`**: the worker catches panics (`catch_unwind` around a build and its
  preparation), and salsa cancels a query by unwinding. Not tried.
- **`opt-level = 3`**: already `release`'s default.
- **`-Ctarget-cpu`**: x86-64-v3 moved nothing measurable (-1% edit work), and a shipped
  binary has to run on any x86-64 and AArch64 machine.
- **One codegen unit, fat LTO**: above.
- **BOLT**: not tried. PGO already lays out the hot code; BOLT would add a Linux-only third
  build step and `llvm-bolt`, which `llvm-tools` does not include.

## Found on the way: workers that outlived their daemon

A worker started ahead of cargo's first build (#11) reads cargo's lines on a thread with
`for line in stdin().lines()`, and on the last one calls `end_with_input`, which reads the
standard input until it closes. `lines()` holds the `StdinLock` for the whole loop, so that
read waited for a lock its own thread held, forever. A daemon that crashed or was killed
left such a worker running with its memory (2 GB on fd) and its socket, and the next daemon's
first build fell back to cargo ("another daemon serves"). Fixed in its own commit.
`blaze_cli.py`'s daemon checks (run alone, macOS): without the fix "the workers of the
killed daemon are gone" and the two checks after it fail, with it they hold. The checks of
that part that fail with and without it: 16 clients at once running the same program while
the daemon splices it (on macOS one of them sticks in `_dyld_start` or is killed, signal 9),
and the three checks of a client of another build of blaze replacing the daemon.

## Verified

macOS, the `dist` build with the fix, `RUST_BACKTRACE` unset: `difftest.py corpus --layouts
--mixed 8` and `mixfutures --mixed 16 --layouts` match, `growth.py` and `movedlines.py` pass,
`cargo test --release -p blaze-engine` passes, `cargo test --release -p hir-ide` fails the 18 tests
it fails on main and no other. `build_release.py` ran through on Linux x86-64 (57 billion
counts, 319 s) and on macOS AArch64 (64 billion counts, 512 s on the loaded Mac), and the
corpus with `--mixed 8` matches under the macOS binary it made. Not run: Linux AArch64,
Windows (not a release target), the LLVM backend.
