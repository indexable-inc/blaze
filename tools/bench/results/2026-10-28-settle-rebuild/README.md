# A worker started from the store in place of the cold one (2026-10-28)

Lane relax-images, [#1299](https://github.com/indexable-inc/blaze/issues/1299), after Rik
Arends' settle rebuild in Relax (`compiler/src/settle_rebuild.rs`). The worker whose first build
compiled from cold writes `settles` beside its socket once the store has what that build kept;
the daemon, after the worker has gone 10 s unasked, lets it go and starts one from the store in
its place (docs/cargo.md, daemon).

## Method

GitHub Actions `ubuntu-24.04` runners: x86_64, 4 cores, 16 GB. No ix VM could be had for the
lane (capacity, then VMs that hung), and anim is private, so the project is a stand-in with
anim's `Cargo.toml` and `Cargo.lock` (94 crates: wgpu, taffy, tiny-skia, cpal, crossterm,
flate2, svgtypes, bytemuck derive) and a `main` that calls into each; it is not in this
directory. RSS is `/proc/<pid>/status` `VmRSS`, HWM its `VmHWM`, of the worker.

- `releases-summary.txt` (`measure.sh`): releases 0.20261028.9 then 0.20261028.10, as
  installed (`ix.dev/blaze`): cold, cold after the upgrade, two fresh daemons on the warm
  store, cold again, and each with `BLAZE_MEMORY`.
- `settle-summary.txt` (`settle.sh`): one engine, `blaze build -p blaze-engine --release
  --features llvm --locked` of abc5d4ca2c in `nix develop`, as CI builds blaze. A cold build,
  two edits on its worker, the swap, three edits on the new worker; then a fresh daemon on the
  warm store for reference. An engine blaze built runs about 10 times slower than a release
  (#899), so its times are its own; the memory is not.

## Numbers

Releases (0.20261028.10, `releases-summary.txt`), after the build and 30 s:

| | worker RSS | HWM | an edit |
|---|---|---|---|
| cold | 4436 MB, 3661 MB after an edit | 5510 MB | 1.09 s, 0.88 s |
| cold after the upgrade from .9 | 4492 MB, 3658 MB | 5529 MB | 0.25 s, 0.04 s |
| fresh daemon, warm store | 2328 MB, 2039 MB | 4245 MB | 0.42 s, 0.05 s |

The cold worker's edits run again proc macros it took to keep state when it loaded them
(222 to 444 of them, 0.83 s of each edit's `apply-us: change=`): the warm one knows from the
store that they keep none.

The swap (`settle-summary.txt`):

| | worker RSS | an edit (`Rebuilt`) |
|---|---|---|
| cold worker | 5630 MB, 5223 MB after an edit | 2.21 s, 1.89 s |
| the worker from the store in its place | 2798 MB, 2206 MB, 2194 MB | 0.63 s, 0.23 s, 0.24 s |
| fresh daemon on the warm store | 2307 MB, 2180 MB, 2169 MB | 0.63 s, 0.22 s, 0.23 s |

The swap was done 129 s after the last edit: 10 s unasked, then the new worker's load and its
base from the store with an engine blaze built. A release's warm start of the stand-in takes
20 s on the same runner (`releases-summary.txt`, warm1).
