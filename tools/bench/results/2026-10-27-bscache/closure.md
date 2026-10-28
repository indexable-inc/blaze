# Build scripts of the bench corpus in their closure, and their runs on another machine

Every build script ran with `BLAZE_SCRIPT_VIEW=on` (`crates/blaze-engine/src/cargo/hermetic`): a
root of its own with its package read-only, its `OUT_DIR` writable, the dependencies' `OUT_DIR`s
it is handed (`DEP_*`), the toolchain and blaze's own rustc, and the pinned nixpkgs closure at
`/nix/store` (clang, lld, binutils, glibc, linux headers, pkg-config, bash, coreutils, make,
sed, awk, grep, diff, find, plus a crate's libraries from `libs.toml` or
`[workspace.metadata.blaze.system-deps]`). No `/usr`, no `$HOME`, no network. Its environment
is cargo's documented variables for build scripts, those it declares with
`rerun-if-env-changed`, the variables fetchlock hands it, and the closure's tools: never the
runner's. A run is kept under the closure key: what the script was built from, its
arguments and environment with the checkout's places unplaced, the package's source, each
closure path's NarHash, and the dependencies' `OUT_DIR` trees.

Made by `harness/bscache.py closure` and `closure-report`, `--jobs 4`, two ix VMs (arm64, 16 cpus):
host A `bscache4` (8 GB), host B `bscache` (4 GB). The closure was fetched from
cache.nixos.org once by `blaze-engine closure fetch` (no nix binary: 203 store paths, 2.68 GB, 23.2 s)
and pushed to a directory remote as project `nix-closure` (0.67 s); each host lays it out from
that remote (`blaze-engine closure materialize`, 4.4 to 5.7 s). The seconds are the whole
`cargo check --compile-time-deps` of the project, most of it compiling scripts and proc macros
(cargo's rustc compiles here, `BLAZE_ONLY_ASKS`), not the runs.

## Host A, empty store (closure-a/)

| project | cargo | seconds | scripts | ok | from the store | failed |
|---|---|---|---|---|---|---|
| axum | 0 | 38.11 | 55 | 55 | 10 | 0 |
| bat | 0 | 11.8 | 28 | 28 | 1 | 0 |
| bevy | 101 | 24.68 | 72 | 71 | 5 | 1 |
| biome | 101 | 69.49 | 62 | 54 | 0 | 8 |
| bun | 101 | 0.13 | 0 | 0 | 0 | 0 |
| cargo | 0 | 16.21 | 41 | 41 | 0 | 0 |
| fd | 0 | 19.95 | 9 | 9 | 1 | 0 |
| helix | 0 | 9.99 | 29 | 29 | 0 | 0 |
| hyperfine | 0 | 5.65 | 20 | 20 | 2 | 0 |
| just | 0 | 5.05 | 18 | 18 | 2 | 0 |
| nushell | 0 | 31.17 | 90 | 90 | 10 | 0 |
| ripgrep | 0 | 3.65 | 10 | 10 | 0 | 0 |
| zed | 101 | 13.16 | 0 | 0 | 0 | 0 |
| total |  |  | 434 | 425 | 31 | 9 |

"From the store" on an empty store is a project's runs another project already made (the
projects run in this order, one store).

- 425 of 434 runs (97.9%) succeed in their closure: 401 with the toolchain and the libc sysroot
  alone, 24 with their `libs.toml` libraries (openssl-sys 3, libgit2-sys 4, libz-sys 4,
  curl-sys 2, libssh2-sys 2, libsqlite3-sys 2, zstd-sys 2, onig_sys, aws-lc-sys, alsa-sys,
  wayland-sys, x11-dl).
- helix-term runs: fetchlock's `HELIX_DISABLE_AUTO_GRAMMAR_BUILD` is handed to it in the view.
- biome's 7 `*_analyze` crates (8 runs) write generated code into their own `src`, which a run
  in its closure can't: the note says so and names the opt-out
  (`unsandboxed-build-scripts`), whose runs are kept on that machine alone.
- `libudev-sys` (bevy) needs `libudev`, which the pinned nixpkgs attributes don't have (systemd
  is not in `pins.txt`); it needs a pin and a `libs.toml` line.
- zed and bun are not measured: zed's `.cargo/config.toml` links with `-fuse-ld=lld`, which the
  VM has no `ld.lld` for (its first proc macro fails to link, before any script), and bun's
  `vendor/lolhtml` submodule is not in its checkout.

## Host A again, same store, a new target directory

All 423 runs that succeeded came from the store (the run before the last fixes:
`res-warm` of commit 8eec9ce89a, 434 runs, 423 ok, 423 from the store).

## Host B, host A's store pulled from the remote (closure-b/)

Host A's store was pushed to the directory remote as project `runs3`
(`blaze-engine cache push`, 0.54 s); host B pulled it into an empty store (`blaze-engine cache pull`,
7.5 s), laid out the closure from the remote into a home of its own, and ran the projects with
the same checkout path as host A, as CI runners have.

| project | cargo | seconds | scripts | ok | from the store | failed |
|---|---|---|---|---|---|---|
| axum | 0 | 26.77 | 55 | 55 | 55 | 0 |
| bat | 0 | 12.68 | 28 | 28 | 28 | 0 |
| bevy | 101 | 28.85 | 72 | 71 | 71 | 1 |
| cargo | 0 | 15.91 | 41 | 41 | 41 | 0 |
| fd | 0 | 4.92 | 9 | 9 | 9 | 0 |
| helix | 0 | 12.37 | 29 | 29 | 29 | 0 |
| hyperfine | 0 | 6.27 | 20 | 20 | 20 | 0 |
| just | 0 | 6.24 | 18 | 18 | 18 | 0 |
| nushell | 0 | 33.01 | 90 | 90 | 90 | 0 |
| ripgrep | 0 | 4.26 | 10 | 10 | 10 | 0 |
| total |  |  | 372 | 371 | 371 | 1 |

371 of 371: every run that succeeds on host B is host A's.

With another checkout path on host B (and so another `CARGO_HOME`), ripgrep had 2 of 10. The
keys' fields (`BLAZE_KEY_TRACE`) differ only in the script itself: a script cargo's rustc
compiled is keyed by its executable's bytes, which hold the paths of its sources and its
dependencies' under `CARGO_HOME`. Its environment, the package, the closure's NarHashes and
the toolchain were the same on both hosts. A script keyed by what it was built from, with the
places unplaced, travels between checkouts too; `own_script`'s made-of key names absolute paths
as well, so it is not that yet.
