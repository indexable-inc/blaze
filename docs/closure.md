---
status: plan
summary: The closure build scripts run with: what is fetched and laid out (built), and the mount namespace (not yet).
date: 2026-10-27
---
# The closure build scripts run with

2026-10-27. Status: fetching, keeping and laying out the closure is built
(`crates/blaze-cargo/src/cargo/closure/`). The mount namespace that shows a build script only this
closure, its package and its `OUT_DIR`, and the table of which crate needs which library, are
other lanes' work and not built yet.

## Why

A build script compiles C, runs `pkg-config`, `cmake` or a shell command, and links system
libraries. Today it gets whatever the machine has, so a build that works on one machine fails
on the next, and a store entry of a script's run can only be shared where machines agree by
luck. The plan (Andrew, 2026-10-27): a build script runs in a Linux mount namespace that sees
only a declared closure: the package's source read-only, its `OUT_DIR` writable, a pinned
toolchain (clang, lld, ar/nm/ranlib, a libc sysroot, pkg-config), a small shell userland (bash, coreutils,
gnumake, gnused, gawk, gnugrep, diffutils, findutils) and the system libraries the crate
declares (openssl, zlib, ...). Everything it can see is pinned, so its run is keyed by what it
saw, and a run CI kept is one every user can take.

The pinned inputs are nixpkgs store paths, taken from nixpkgs' binary cache, because nixpkgs
already builds every one of them for both Linux hosts, reproducibly, under one revision, with
every path's dependencies named. Nobody needs nix for it, and no blaze code path runs a `nix`
binary: blaze reads the cache's format itself.

## What is pinned

`crates/blaze-cargo/src/cargo/closure/pins.txt`, written by `python3 harness/closure_pins.py
--write` (in an ix VM, where nix is: the script runs `nix eval` on the pinned revision to name
each root's store path, and reads each path's NarHash and closure from cache.nixos.org's
`.narinfo` files; it is a tool for moving the pin, not a part of blaze). It names:

- the nixpkgs commit, `3ec636362e1816bc2f0888fa0fa2c69f1bc9e504` (nixos-unstable on
  2026-10-27; `--rev latest` moves it to the channel's newest);
- for each of `x86_64-linux` and `aarch64-linux`, each root by its attribute (`clang`, `lld`,
  `llvmPackages.bintools`, `glibc.out`, `glibc.dev`, `linuxHeaders`, `pkg-config`, the userland, `cmake`, `git`, and
  `openssl.out`, `openssl.dev`, `zlib.dev`, `libgit2.lib`, ... `protobuf.out`), with its store
  path, the NarHash and NarSize cache.nixos.org signs for it, and what its closure comes to.

`clang` is the wrapped compiler (`clang-wrapper`), so that `CC=clang` finds its sysroot,
`llvmPackages.bintools` the wrapped LLVM binutils (`ar`, `nm`, `ranlib`, `objcopy`, ...) and
`pkg-config` the wrapped one. A library is pinned as each output the per-crate table may name:
`openssl.dev` has the headers and `.pc` files, and refers to `openssl.out`, which has the
libraries.

What the closures come to, as NARs (unpacked bytes) and store paths, from pins.txt:

| host | all roots | clang | lld | bintools | git | cmake | glibc.dev | openssl.dev |
|---|---|---|---|---|---|---|---|---|
| x86_64-linux | 2,672.9 MB, 204 paths | 2,120.9 MB, 48 | 672.1 MB, 11 | 825.1 MB, 24 | 404.3 MB, 87 | 158.4 MB, 27 | 51.3 MB, 7 | 51.0 MB, 7 |
| aarch64-linux | 2,679.7 MB, 203 paths | 2,132.4 MB, 48 | 663.2 MB, 11 | 806.1 MB, 23 | 424.1 MB, 87 | 162.1 MB, 27 | 62.7 MB, 7 | 62.2 MB, 7 |

Closures overlap (every one has glibc), so a column is not a share of the total. clang's is
most of it: the wrapper refers to clang, LLVM's libraries, compiler-rt and GCC's (for
libstdc++).

## From cache.nixos.org to blaze's store: CI only

`blaze-engine closure fetch [--host HOST] [ROOT...]`, every root of the host by default:

1. Each root's `<hash>.narinfo` is read from cache.nixos.org (or `BLAZE_NIX_CACHE`, an address
   or a directory laid out as a cache is), then each path its `References` name, to the whole
   closure. A narinfo is taken only if its `Sig` by `cache.nixos.org-1` (public key
   `6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=`) holds for nix's fingerprint
   `1;<StorePath>;<NarHash>;<NarSize>;<References as whole paths, sorted, joined by commas>`,
   and a root's NarHash must be the one pins.txt has. A path whose manifest the store has
   already is not asked for again.
2. Each NAR is downloaded (`nar/<file>.nar.zst` or `.xz`), decompressed and read as it streams
   in, eight at a time. The file's sha256 and size are checked against `FileHash` and
   `FileSize`, and the NAR's against `NarHash` (nix's base32: the alphabet
   `0123456789abcdfghijklmnpqrsvwxyz`, written from the hash's last bits) and `NarSize`. A
   download that fails is tried again from the start, four times in all.
3. What the NAR holds goes into blaze's store as it is read (below). The manifest of the path is
   added only once its NAR is known to be the one signed, and once all are in, the store is
   sealed and every entry added is looked up again: an entry the store did not keep fails the
   fetch.

Then CI sends the store to the remote cache like any other: `blaze-engine cache push REMOTE
nix-closure`, from a store that holds only the closure, so that the project `nix-closure` lists
only its packs. The gateway needs no change: these are ordinary packs.

## How the store keeps a store path

- **Pieces.** Each file's bytes are cut into pieces of 1 MiB (`blaze_store::BLOCK_MAX`), the
  last shorter, each a store entry keyed by `sha256(len, "nix file piece 1", len, bytes)`. The
  store packs a value of up to 1 MiB in a zstd block; a larger one would be kept as it is, so
  1 MiB is the largest piece that is compressed. Pieces are keyed by their bytes, so a file two
  store paths share, or two outputs of one package, is kept once.
- **Manifests.** Each store path is one entry keyed by
  `sha256(len, "nix store path 1", len, <store path>, len, <NarHash>)`: the path, its NarHash and
  NarSize, the store paths it refers to with theirs, and its entries in NAR order, each its path
  from the root, its kind (directory, file, program, link) and its pieces or link target.
  Because the references carry their NarHashes, a machine with only pins.txt and the store
  walks the whole closure.
- **Lasting keys.** An entry of the store is otherwise keyed by the build of blaze that wrote it
  and its toolchain ([remote-cache.md](remote-cache.md#what-exists-today)), so every commit of
  blaze would make CI fetch the 2.5 GB again. These keys name everything their values are made
  from, so they are kept under `Store::insert_lasting`, whose identity is the store's format
  alone: every build of blaze finds them.

## From blaze's store to a directory: everyone

`blaze-engine closure materialize [--host HOST] [ROOT...]` walks the roots' closure in the store and
lays each store path out as `~/.blaze/nix/<hash>-<name>` (`BLAZE_HOME/nix`), printing each store
path and its directory. With `BLAZE_REMOTE` set it first reads which pack of the project
`nix-closure` holds which key, and the store fetches a pack when a key of it is first asked for.
It never reads cache.nixos.org: a path the store lacks and no remote brings is an error that
names it and says CI fills the remote with it.

A path is written to `.<hash>-<name>.unpacking` beside its place, its pieces read from the
store a batch at a time, files dated one second after 1970 as nix dates its store, then made
read-only; its NAR is written again from what is on disk and hashed, and only a tree whose NAR
is the NarHash cache.nixos.org signed is renamed into place. A damaged entry is an error, and
nothing is put in place. A path that is in place is not laid out again; two processes that ask
at once lay it out once.

On Linux the namespace mounts each directory at `/nix/store/<hash>-<name>`, where nixpkgs' own
paths in its programs and links point.

## The API the other lanes use

In `crates/blaze-cargo/src/cargo/closure/mod.rs`:

- `roots(host: &str) -> Vec<Root>`: pins.txt's roots for `x86_64-linux` or `aarch64-linux`,
  each its `attribute` (`openssl.dev`), its `StorePath` and its closure's size. `host()` is this
  machine's, `nixpkgs()` the pinned commit.
- `closure_of(roots: &[StorePath]) -> anyhow::Result<Vec<StorePath>>`: the whole closure, sorted,
  each with its NarHash and NarSize, from the store: what a key of a run that sees it names.
- `materialize(path: &StorePath) -> anyhow::Result<PathBuf>`: the path laid out, as above.

They use the process's store (`BLAZE_CACHE`, else the daemon's) and whatever the store asks
for what it lacks: a process that is to take the closure from a remote opens it first, as
`closure materialize` does with `remote::Shared::lazily(store, remote, "nix-closure")`.

## Measured

The `aarch64-linux` closure, every root before `llvmPackages.bintools` was pinned (200 paths;
it adds 3, 123 MB of NARs), on an arm64 ix VM (16 CPUs, 16 GiB), with a release
build of blaze; the remote is a directory on the same disk, so the pull measures blaze, not a
network:

| step | time | bytes |
|---|---|---|
| `closure fetch`, empty store, from cache.nixos.org | 42.1 s | 607.2 MB downloaded (zstd NARs), 2,556.7 MB of NARs, 200 paths |
| `closure fetch` again | 0.16 s | nothing downloaded: 200 manifests in the store |
| what the store keeps | | 638.6 MB in 39 files |
| `cache push DIR nix-closure` | 0.54 s | 37 packs, 638.6 MB |
| `closure materialize`, empty store, `BLAZE_REMOTE=DIR` | 29.7 s | 24,720 keys in 37 packs pulled; 2,550.5 MB laid out |
| `closure materialize` again, all in place | 0.15 s | |
| `closure materialize` from the filled store, no remote | 15.5 s | 2,550.5 MB laid out |

The store keeps the 2.5 GB in about what cache.nixos.org sends (zstd blocks of 1 MiB pieces,
pieces shared across paths kept once). Laying out is most of a materialize: each tree is
written, then read again and hashed as a NAR before it is renamed into place.

Of the 135 paths the VM's own `/nix/store` also has, 134 dump (`nix-store --dump`) to the same
bytes as blaze's tree. The other, `ncurses-6.6`, the VM had built itself, unsigned; blaze's
tree is the one whose NarHash cache.nixos.org signs. With an empty store and no remote,
`closure materialize openssl.dev` fails with the path and the note that CI fills the remote.

## What is left

- The mount namespace and the build-script runner (lane `nsroot`), and the per-crate table.
- A CI job that runs `blaze-engine closure fetch` for both hosts into a fresh store and pushes it as
  `nix-closure`, on each move of the pin.
- A process takes keys it lacks from one remote project: the daemon pulls its own project, so
  for build scripts it runs, the closure's project is to be read beside it.
- The store's garbage collection does not know the closure from other packs: on a small disk,
  `BLAZE_CACHE_MAX` (default a tenth of the disk) can be below the closure's packs.
