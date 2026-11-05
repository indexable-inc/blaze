# Cold builds from an empty store: the scripts' standard library made once (#203)

A cold `blaze build` with an empty store and `BLAZE_STD_FROM=off`: what a build of
blaze that is not a release, or a toolchain the release shipped no standard library for, starts
from. Each build-script and proc-macro build (`own_script.rs`) is a `blaze` process of its own;
with nothing in the store, each made the standard library's unit and the frozen maps of its
crates itself, all at once. With the change, the first makes it under a lock in the store and
the others wait for it and read it from the store.

ix VM, arm64 Neoverse-V3, 16 vCPU, 16 GiB committed, cranelift, nightly-2026-10-25; main
b6a385c8de against the branch. Each run in a systemd unit of its own with `MemoryMax=13G` and no
swap, alternating branch and main, one run each. Peak is the largest total RSS of blaze's
processes sampled every 50 ms (`procsample.py`; the JSON files here); unit peak is the cgroup's.

| project | arm | wall | peak (sampled) | unit peak | builds that made the standard library |
|---|---|---|---|---|---|
| hyperfine | main | OOM-killed at 10.7 s | | 13 GB (the cap) | |
| hyperfine | branch | 52.5 s | 7,853 MB | 8.2 GB | 1 of 22 |
| ripgrep | main | 41.7 s | 9,173 MB | 9.1 GB | 7 of 8 |
| ripgrep | branch | 41.5 s | 4,386 MB | 5.3 GB | 1 of 8 |

The same main build of hyperfine without the cap wedged a 16 GiB VM (load 98, 13.8 GB used)
until ix marked it failed.

One script's build alone, with the standard library already in the store, peaks at 619 MB
(0.63 s); the 16 that hyperfine's cargo starts together are most of the 7.9 GB left
(script builds: 16 processes, 7,015 MB anonymous). The same build peaks at 385 MB on 4 cores
and 255 MB on 1, at about the same wall time, and at 439 MB with one jemalloc arena: the
threads each script build starts are what is left to bound.
