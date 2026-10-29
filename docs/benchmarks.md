---
status: current
summary: How long an edit takes to build, the memory and disk it uses and how fast its programs run, against cargo, on three platforms.
---
# Benchmarks

How long an edit takes to build under blaze, how much memory and disk it uses, and how fast the
programs it builds run, against cargo. Each table is a matrix over three platforms, macOS arm64,
Linux aarch64 and Linux x86-64, each measured on a dedicated machine of its own, on real
projects at pinned commits, against plain cargo on the same machine, and written back here (and
the README's numbers with them) by a bot commit. A platform's numbers compare with each other
and with its own history, not with another platform's: the machines differ in more than their
architecture. The rest of this page says how, so that anyone can check it or run it again.

- [How fast an edit builds](#how-fast-an-edit-builds)
- [How much memory and disk it takes](#how-much-memory-and-disk-it-takes)
- [How fast the built programs run](#how-fast-the-built-programs-run)
- [What is measured](#what-is-measured), [the machines](#the-machines),
  [run it yourself](#run-it-yourself) and [every number](#results)

## How fast an edit builds

<!-- benchsummary:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="benchmarks/charts/edits-dark.svg"><img alt="How much faster an edit builds under blaze than under cargo, by platform: All projects (geometric mean): macOS arm64 10× faster, Linux aarch64 24× faster, Linux x86-64 11× faster; ripgrep 15.1.0: macOS arm64 8.1× faster, Linux aarch64 23× faster, Linux x86-64 12× faster; fd 10.3.0: macOS arm64 6.0× faster, Linux aarch64 22× faster, Linux x86-64 9.3× faster; bat 0.26.0: Linux aarch64 33× faster, Linux x86-64 14× faster; hyperfine 1.19.0: Linux aarch64 14× faster, Linux x86-64 8.0× faster; just 1.43.0: Linux aarch64 22× faster, Linux x86-64 14× faster; axum 0.8.6: Linux aarch64 16× faster, Linux x86-64 10× faster; nushell 0.108.0: macOS arm64 10× faster, Linux aarch64 34× faster, Linux x86-64 20× faster; helix 25.07.1: Linux aarch64 48× faster, Linux x86-64 19× faster; bevy 0.20.0: Linux aarch64 26× faster, Linux x86-64 3.7× faster; biome @biomejs/biome@2.6.3: Linux x86-64 5.6× faster; bun 1.4.3: macOS arm64 21× faster, Linux aarch64 22× faster, Linux x86-64 22× faster" src="benchmarks/charts/edits-light.svg" width="760"></picture>

Edit, then build, on real projects at pinned commits: incremental `cargo build` → `blaze build`, both warm, same linker, Cranelift, the median of each run's edits. Each platform is a machine of its own: compare numbers within a platform, not across.

An edit to one function, cargo → blaze:

| Project | macOS arm64 | Linux aarch64 | Linux x86-64 |
|---|---:|---:|---:|
| ripgrep 15.1.0 | 993 ms → **126 ms** (7.9x) | 1.18 s → **62 ms** (19x) | 508 ms → **56 ms** (9.1x) |
| fd 10.3.0 | 630 ms → **109 ms** (5.8x) | 1.27 s → **58 ms** (22x) | 494 ms → **53 ms** (9.3x) |
| bat 0.26.0 | - | 1.62 s → **56 ms** (29x) | 502 ms → **49 ms** (10x) |
| hyperfine 1.19.0 | - | 753 ms → **38 ms** (20x) | 385 ms → **35 ms** (11x) |
| just 1.43.0 | - | 1.51 s → **43 ms** (35x) | 707 ms → **41 ms** (17x) |
| axum 0.8.6 | - | 803 ms → **51 ms** (16x) | 427 ms → **42 ms** (10x) |
| nushell 0.108.0 | 1.81 s → **454 ms** (4.0x) | 7.20 s → **323 ms** (22x) | 1.25 s → **236 ms** (5.3x) |
| helix 25.07.1 | - | 6.84 s → **82 ms** (83x) | 2.15 s → **116 ms** (19x) |
| bevy 0.20.0 | - | 22.81 s → **884 ms** (26x) | 3.20 s → **874 ms** (3.7x) |
| biome @biomejs/biome@2.6.3 | - | - | 2.12 s → **2.38 s** (0.9x) |
| bun 1.4.3 | 7.55 s → **362 ms** (21x) | 10.63 s → **486 ms** (22x) | 9.11 s → **451 ms** (20x) |

- **macOS arm64**: MacBook Pro (Mac17,6), macOS 27.0.1, with other work running (Apple M5 Max, 18 cores, 128 GiB), blaze [`84760c3f51`](https://github.com/indexable-inc/blaze/commit/84760c3f517a76f6e8a3aa41ccd81fc3f26b87c7) on 2026-10-21, median of 3 edits, load average 48 at the start, by hand.
- **Linux aarch64**: AWS c8g.16xlarge, pinned to 16 cores with taskset (Neoverse-V2, 16 of its 64 cores, 124 GiB), blaze [`c93e530e47`](https://github.com/indexable-inc/blaze/commit/c93e530e474c4de2d67a7a9ec49fd5581894efb8) on 2026-10-23, median of 5 edits, by hand.
- **Linux x86-64**: AWS c8a.metal-24xl (dev-aws-3), pinned to 16 cores with taskset (AMD EPYC 9R45, 16 of its 96 cores, 188 GiB), blaze [`1a005421b7`](https://github.com/indexable-inc/blaze/commit/1a005421b7a47fa3968bc5de6645e25a50dfd855) on 2026-10-25, median of 5 edits, by hand.
- **bun** is measured on its own, by the `bench-bun` workflow: macOS arm64 on one machine (Apple M5 Max, 18 cores, 128 GiB), blaze [`27077156cc`](https://github.com/indexable-inc/blaze/commit/27077156ccbd476079d261e529fc25b3225f0ace) on 2026-10-23, median of 3 edits, by hand; Linux aarch64 on AWS c8g.24xlarge (dev-aws-eph-blaze-arm64bun), pinned to 16 cores with taskset (Neoverse-V2, 16 of its 96 cores, 185 GiB), blaze [`ac1ee2dd68`](https://github.com/indexable-inc/blaze/commit/ac1ee2dd682b7d321ba75a67924aaf95609572f0) on 2026-10-23, median of 3 edits, by hand; Linux x86-64 on AWS c8a.24xlarge (dev-aws-eph-blaze-x86edits), pinned to 16 cores with taskset (AMD EPYC 9R45, 16 of its 96 cores, 185 GiB), blaze [`961bb3105a`](https://github.com/indexable-inc/blaze/commit/961bb3105a8e90e0ac33488c803f5513f075bb87) on 2026-10-23, median of 3 edits, load average 12 at the start, by hand. It is the Rust port of [bun](https://github.com/oven-sh/bun), 1.25M lines of Rust in 103 crates on a nightly beside a JavaScript engine's C and C++, built with frame pointers as bun's own build is (`-C force-frame-pointers=yes`; [what it took](research/bun.md)). Its test suites are not measured yet.

<details><summary>Every kind of edit, by platform</summary>

**macOS arm64**

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 993 ms → **126 ms** (7.9x) | 1.31 s → **159 ms** (8.2x) | 828 ms → **175 ms** (4.7x) | 67 ms → **54 ms** (1.2x) |
| fd 10.3.0 | 630 ms → **109 ms** (5.8x) | 734 ms → **102 ms** (7.2x) | 681 ms → **113 ms** (6.0x) | 144 ms → **49 ms** (2.9x) |
| nushell 0.108.0 | 1.81 s → **454 ms** (4.0x) | 14.39 s → **850 ms** (17x) | 10.28 s → **716 ms** (14x) | 407 ms → **45 ms** (9.0x) |
| bun 1.4.3 | 7.55 s → **362 ms** (21x) | 16.01 s → **457 ms** (35x) | 7.26 s → **823 ms** (8.8x) | 199 ms → **122 ms** (1.6x) |

**Linux aarch64**

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.18 s → **62 ms** (19x) | 1.54 s → **57 ms** (27x) | 1.34 s → **101 ms** (13x) | 25 ms → **18 ms** (1.4x) |
| fd 10.3.0 | 1.27 s → **58 ms** (22x) | 1.24 s → **35 ms** (35x) | 1.29 s → **64 ms** (20x) | 34 ms → **17 ms** (2.0x) |
| bat 0.26.0 | 1.62 s → **56 ms** (29x) | 1.92 s → **55 ms** (35x) | 1.87 s → **57 ms** (33x) | 65 ms → **17 ms** (3.8x) |
| hyperfine 1.19.0 | 753 ms → **38 ms** (20x) | 797 ms → **59 ms** (14x) | 806 ms → **93 ms** (8.7x) | 44 ms → **17 ms** (2.6x) |
| just 1.43.0 | 1.51 s → **43 ms** (35x) | 1.53 s → **71 ms** (22x) | 1.50 s → **84 ms** (18x) | 43 ms → **17 ms** (2.5x) |
| axum 0.8.6 | 803 ms → **51 ms** (16x) | 1.37 s → **43 ms** (32x) |  | 73 ms → **18 ms** (4.1x) |
| nushell 0.108.0 | 7.20 s → **323 ms** (22x) | 11.36 s → **295 ms** (39x) | 11.32 s → **371 ms** (31x) | 192 ms → **21 ms** (9.1x) |
| helix 25.07.1 | 6.84 s → **82 ms** (83x) | 8.20 s → **95 ms** (86x) | 7.59 s → **267 ms** (28x) | 92 ms → **18 ms** (5.1x) |
| bevy 0.20.0 | 22.81 s → **884 ms** (26x) | 32.13 s → **352 ms** (91x) |  | 217 ms → **24 ms** (9.0x) |
| bun 1.4.3 | 10.63 s → **486 ms** (22x) | 19.64 s → **477 ms** (41x) | 10.67 s → **629 ms** (17x) | 90 ms → **21 ms** (4.3x) |

**Linux x86-64**

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 508 ms → **56 ms** (9.1x) | 781 ms → **53 ms** (15x) | 625 ms → **88 ms** (7.1x) | 22 ms → **16 ms** (1.4x) |
| fd 10.3.0 | 494 ms → **53 ms** (9.3x) | 459 ms → **33 ms** (14x) | 498 ms → **57 ms** (8.7x) | 33 ms → **15 ms** (2.2x) |
| bat 0.26.0 | 502 ms → **49 ms** (10x) | 730 ms → **49 ms** (15x) | 689 ms → **51 ms** (14x) | 60 ms → **15 ms** (4.0x) |
| hyperfine 1.19.0 | 385 ms → **35 ms** (11x) | 417 ms → **52 ms** (8.0x) | 423 ms → **76 ms** (5.6x) | 41 ms → **15 ms** (2.7x) |
| just 1.43.0 | 707 ms → **41 ms** (17x) | 729 ms → **60 ms** (12x) | 711 ms → **71 ms** (10x) | 41 ms → **15 ms** (2.7x) |
| axum 0.8.6 | 427 ms → **42 ms** (10x) | 865 ms → **35 ms** (25x) |  | 74 ms → **16 ms** (4.6x) |
| nushell 0.108.0 | 1.25 s → **236 ms** (5.3x) | 4.75 s → **200 ms** (24x) | 4.83 s → **278 ms** (17x) | 180 ms → **18 ms** (10x) |
| helix 25.07.1 | 2.15 s → **116 ms** (19x) | 3.29 s → **67 ms** (49x) | 2.80 s → **217 ms** (13x) | 83 ms → **16 ms** (5.2x) |
| bevy 0.20.0 | 3.20 s → **874 ms** (3.7x) | 11.60 s → **699 ms** (17x) |  | 183 ms → **19 ms** (9.6x) |
| biome @biomejs/biome@2.6.3 | 2.12 s → **2.38 s** (0.9x) | 9.86 s → **381 ms** (26x) | 8.06 s → **108.75 s** (c) (0.1x) | 205 ms → **32 ms** (6.4x) |
| bun 1.4.3 | 9.11 s → **451 ms** (20x) | 16.51 s → **430 ms** (38x) | 9.22 s → **556 ms** (17x) | 72 ms → **18 ms** (4.0x) |

</details>

(c): the daemon handed this edit to cargo (a fallback, so about cargo's time).

Measured by hand with `harness/bench.py`, or by the `bench-readme` workflow (a project measured on its own, by its own workflow) on each platform's benchmark machine once it is registered. The blaze measured is `cargo build --release`; the binaries a release ships are the `dist` profile with PGO, which start and edit faster ([how](https://github.com/indexable-inc/blaze/blob/main/docs/reports/RELEASE-BUILD.md)). Cold builds, the spread of every number, the LLVM tier and how fast the built programs run: [every number below](#results). Raw JSON: [macOS arm64](benchmarks/macos-arm64.json), [Linux aarch64](benchmarks/linux-aarch64.json), [Linux x86-64](benchmarks/linux-x86-64.json); bun: [macOS arm64](benchmarks/bun-macos-arm64.json), [Linux aarch64](benchmarks/bun-linux-aarch64.json), [Linux x86-64](benchmarks/bun-linux-x86-64.json).
<!-- benchsummary:end -->

## How much memory and disk it takes

<!-- benchmem:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="benchmarks/charts/memory-dark.svg"><img alt="Peak memory while an edit to one function builds, blaze against cargo, by platform: All projects (geometric mean): macOS arm64 12× more, Linux aarch64 3.9× more, Linux x86-64 3.2× more; ripgrep 15.1.0: macOS arm64 12× more · 2.9 GB vs cargo's 235 MB, Linux aarch64 3.6× more · 1.7 GB vs cargo's 464 MB, Linux x86-64 3.1× more · 1.7 GB vs cargo's 528 MB; fd 10.3.0: macOS arm64 12× more · 2.6 GB vs cargo's 212 MB, Linux aarch64 2.9× more · 1.5 GB vs cargo's 520 MB, Linux x86-64 2.4× more · 1.5 GB vs cargo's 605 MB; bat 0.26.0: Linux aarch64 2.9× more · 2.0 GB vs cargo's 672 MB, Linux x86-64 2.7× more · 2.0 GB vs cargo's 747 MB; hyperfine 1.19.0: Linux aarch64 4.9× more · 1.8 GB vs cargo's 361 MB, Linux x86-64 3.0× more · 1.6 GB vs cargo's 534 MB; just 1.43.0: Linux aarch64 4.7× more · 2.1 GB vs cargo's 434 MB, Linux x86-64 3.6× more · 1.9 GB vs cargo's 535 MB; axum 0.8.6: Linux aarch64 4.8× more · 1.9 GB vs cargo's 399 MB, Linux x86-64 3.5× more · 1.9 GB vs cargo's 534 MB; nushell 0.108.0: macOS arm64 12× more · 12.5 GB vs cargo's 1.1 GB, Linux aarch64 3.2× more · 7.6 GB vs cargo's 2.4 GB, Linux x86-64 2.7× more · 7.2 GB vs cargo's 2.7 GB; helix 25.07.1: Linux aarch64 3.7× more · 6.6 GB vs cargo's 1.8 GB, Linux x86-64 2.7× more · 5.8 GB vs cargo's 2.1 GB; bevy 0.20.0: Linux aarch64 2.6× more · 17.8 GB vs cargo's 6.7 GB, Linux x86-64 3.4× more · 28.9 GB vs cargo's 8.5 GB; biome @biomejs/biome@2.6.3: Linux x86-64 2.5× more · 7.6 GB vs cargo's 3.0 GB; bun 1.4.3: macOS arm64 12× more · 26.1 GB vs cargo's 2.1 GB, Linux aarch64 7.9× more · 27.7 GB vs cargo's 3.5 GB, Linux x86-64 7.4× more · 24.5 GB vs cargo's 3.3 GB" src="benchmarks/charts/memory-light.svg" width="760"></picture>

<picture><source media="(prefers-color-scheme: dark)" srcset="benchmarks/charts/disk-dark.svg"><img alt="What a project's build keeps on disk, blaze against cargo, by platform: All projects (geometric mean): macOS arm64 1.5× more, Linux aarch64 1.2× more, Linux x86-64 2.0× more; ripgrep 15.1.0: macOS arm64 1.4× more · 598 MB vs cargo's 416 MB, Linux aarch64 1.3× more · 518 MB vs cargo's 404 MB, Linux x86-64 2.1× more · 852 MB vs cargo's 407 MB; fd 10.3.0: macOS arm64 1.3× more · 460 MB vs cargo's 355 MB, Linux aarch64 1.2× more · 801 MB vs cargo's 678 MB, Linux x86-64 2.3× more · 1.3 GB vs cargo's 571 MB; bat 0.26.0: Linux aarch64 1.2× more · 1.2 GB vs cargo's 980 MB, Linux x86-64 2.1× more · 2.0 GB vs cargo's 973 MB; hyperfine 1.19.0: Linux aarch64 1.3× more · 432 MB vs cargo's 340 MB, Linux x86-64 3.4× more · 1.2 GB vs cargo's 342 MB; just 1.43.0: Linux aarch64 1.2× more · 784 MB vs cargo's 664 MB, Linux x86-64 2.3× more · 1.5 GB vs cargo's 667 MB; axum 0.8.6: Linux aarch64 1.1× more · 454 MB vs cargo's 405 MB, Linux x86-64 2.0× more · 825 MB vs cargo's 404 MB; nushell 0.108.0: macOS arm64 1.3× more · 6.9 GB vs cargo's 5.3 GB, Linux aarch64 1.0× · 5.6 GB vs cargo's 5.3 GB, Linux x86-64 1.5× more · 7.8 GB vs cargo's 5.3 GB; helix 25.07.1: Linux aarch64 1.3× more · 4.6 GB vs cargo's 3.7 GB, Linux x86-64 1.6× more · 5.8 GB vs cargo's 3.7 GB; bevy 0.20.0: Linux aarch64 1.1× less · 16.0 GB vs cargo's 17.5 GB, Linux x86-64 1.1× more · 19.2 GB vs cargo's 17.5 GB; biome @biomejs/biome@2.6.3: Linux x86-64 3.9× more · 42.5 GB vs cargo's 11.0 GB; bun 1.4.3: macOS arm64 1.9× more · 11.3 GB vs cargo's 6.0 GB, Linux aarch64 1.2× more · 6.7 GB vs cargo's 5.6 GB, Linux x86-64 1.2× more · 6.7 GB vs cargo's 5.7 GB" src="benchmarks/charts/disk-light.svg" width="760"></picture>

Memory and disk, Cranelift, from the same runs as the build table. Peak: the most every process doing the work held at once, cargo → blaze. For blaze that is the daemon, its workers and what they run, and `blaze build` with any cargo it ran; for plain cargo, cargo itself, rustc, build scripts and the linker. How it is measured: [docs/benchmarks.md](#memory-and-disk).

Peak while an edit to one function builds, cargo → blaze:

| Project | macOS arm64 | Linux aarch64 | Linux x86-64 |
|---|---:|---:|---:|
| ripgrep 15.1.0 | 235 MB → **2.9 GB** | 464 MB → **1.7 GB** | 528 MB → **1.7 GB** |
| fd 10.3.0 | 212 MB → **2.6 GB** | 520 MB → **1.5 GB** | 605 MB → **1.5 GB** |
| bat 0.26.0 | - | 672 MB → **2.0 GB** | 747 MB → **2.0 GB** |
| hyperfine 1.19.0 | - | 361 MB → **1.8 GB** | 534 MB → **1.6 GB** |
| just 1.43.0 | - | 434 MB → **2.1 GB** | 535 MB → **1.9 GB** |
| axum 0.8.6 | - | 399 MB → **1.9 GB** | 534 MB → **1.9 GB** |
| nushell 0.108.0 | 1.1 GB → **12.5 GB** | 2.4 GB → **7.6 GB** | 2.7 GB → **7.2 GB** |
| helix 25.07.1 | - | 1.8 GB → **6.6 GB** | 2.1 GB → **5.8 GB** |
| bevy 0.20.0 | - | 6.7 GB → **17.8 GB** | 8.5 GB → **28.9 GB** |
| biome @biomejs/biome@2.6.3 | - | - | 3.0 GB → **7.6 GB** |
| bun 1.4.3 | 2.1 GB → **26.1 GB** | 3.5 GB → **27.7 GB** | 3.3 GB → **24.5 GB** |

<details><summary>The daemon between edits, cold builds and disk, by platform</summary>

**macOS arm64**

| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 235 MB → **2.9 GB** | 2.2 GB ready, 2.8 GB after 39 builds | 541 MB → **2.4 GB** | 416 MB → 598 MB |
| fd 10.3.0 | 212 MB → **2.6 GB** | 1.8 GB ready, 2.6 GB after 33 builds | 897 MB → **1.9 GB** | 355 MB → 460 MB |
| nushell 0.108.0 | 1.1 GB → **12.5 GB** | 11.5 GB ready, 12.5 GB after 39 builds | 2.4 GB → **16.1 GB** | 5.3 GB → 6.9 GB |
| bun 1.4.3 | 2.1 GB → **26.1 GB** | 11.8 GB ready, 27.8 GB after 33 builds | 3.1 GB → **13.7 GB** | 6.0 GB → 11.3 GB |

**Linux aarch64**

| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 464 MB → **1.7 GB** | 1.5 GB ready, 1.6 GB after 65 builds | 1.9 GB → **2.5 GB** | 404 MB → 518 MB |
| fd 10.3.0 | 520 MB → **1.5 GB** | 1.7 GB ready, 1.5 GB after 55 builds | 2.2 GB → **3.3 GB** | 678 MB → 801 MB |
| bat 0.26.0 | 672 MB → **2.0 GB** | 1.9 GB ready, 1.9 GB after 55 builds | 3.4 GB → **5.8 GB** | 980 MB → 1.2 GB |
| hyperfine 1.19.0 | 361 MB → **1.8 GB** | 1.9 GB ready, 1.7 GB after 55 builds | 2.2 GB → **2.5 GB** | 340 MB → 432 MB |
| just 1.43.0 | 434 MB → **2.1 GB** | 1.8 GB ready, 2.0 GB after 55 builds | 3.0 GB → **3.6 GB** | 664 MB → 784 MB |
| axum 0.8.6 | 399 MB → **1.9 GB** | 2.0 GB ready, 1.9 GB after 35 builds | 2.1 GB → **3.1 GB** | 405 MB → 454 MB |
| nushell 0.108.0 | 2.4 GB → **7.6 GB** | 5.2 GB ready, 7.5 GB after 65 builds | 4.5 GB → **7.5 GB** | 5.3 GB → 5.6 GB |
| helix 25.07.1 | 1.8 GB → **6.6 GB** | 3.7 GB ready, 6.6 GB after 55 builds | 3.5 GB → **5.8 GB** | 3.7 GB → 4.6 GB |
| bevy 0.20.0 | 6.7 GB → **17.8 GB** | 8.1 GB ready, 17.7 GB after 35 builds | 6.8 GB → **9.3 GB** | 17.5 GB → 16.0 GB |
| bun 1.4.3 | 3.5 GB → **27.7 GB** | 12.3 GB ready, 27.7 GB after 33 builds | 4.5 GB → **15.8 GB** | 5.6 GB → 6.7 GB |

**Linux x86-64**

| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 528 MB → **1.7 GB** | 1.6 GB ready, 1.6 GB after 65 builds | 2.4 GB → **5.2 GB** | 407 MB → 852 MB |
| fd 10.3.0 | 605 MB → **1.5 GB** | 1.6 GB ready, 1.4 GB after 55 builds | 2.6 GB → **6.1 GB** | 571 MB → 1.3 GB |
| bat 0.26.0 | 747 MB → **2.0 GB** | 1.9 GB ready, 2.0 GB after 55 builds | 3.7 GB → **7.9 GB** | 973 MB → 2.0 GB |
| hyperfine 1.19.0 | 534 MB → **1.6 GB** | 1.8 GB ready, 1.5 GB after 55 builds | 2.6 GB → **9.6 GB** | 342 MB → 1.2 GB |
| just 1.43.0 | 535 MB → **1.9 GB** | 1.8 GB ready, 1.9 GB after 55 builds | 3.5 GB → **8.6 GB** | 667 MB → 1.5 GB |
| axum 0.8.6 | 534 MB → **1.9 GB** | 2.0 GB ready, 1.8 GB after 35 builds | 2.5 GB → **5.8 GB** | 404 MB → 825 MB |
| nushell 0.108.0 | 2.7 GB → **7.2 GB** | 5.2 GB ready, 7.1 GB after 65 builds | 4.8 GB → **9.1 GB** | 5.3 GB → 7.8 GB |
| helix 25.07.1 | 2.1 GB → **5.8 GB** | 4.2 GB ready, 5.7 GB after 55 builds | 3.8 GB → **8.4 GB** | 3.7 GB → 5.8 GB |
| bevy 0.20.0 | 8.5 GB → **28.9 GB** | 8.2 GB ready, 15.2 GB after 35 builds | 8.5 GB → **9.7 GB** | 17.5 GB → 19.2 GB |
| biome @biomejs/biome@2.6.3 | 3.0 GB → **7.6 GB** | 6.9 GB ready, 19.3 GB after 57 builds | 8.6 GB → **9.6 GB** | 11.0 GB → 42.5 GB |
| bun 1.4.3 | 3.3 GB → **24.5 GB** | 10.5 GB ready, 24.5 GB after 33 builds | 4.5 GB → **14.1 GB** | 5.7 GB → 6.7 GB |

On disk: cargo's target directory → blaze's target directory, store and home.

</details>
<!-- benchmem:end -->

## How fast the built programs run

<!-- runbench:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="benchmarks/charts/runtime-dark.svg"><img alt="How fast each benchmark runs built by blaze, against cargo build --release, by platform: All benchmarks (geometric mean): macOS arm64 1.18× slower, Linux aarch64 1.14× slower, Linux x86-64 1.10× slower; sieve: macOS arm64 1.12× slower, Linux aarch64 1.07× slower, Linux x86-64 1.00×; collatz: macOS arm64 1.01× slower, Linux aarch64 1.00×, Linux x86-64 1.00×; fnv: macOS arm64 1.00×, Linux aarch64 1.01× slower, Linux x86-64 1.00×; qsort: macOS arm64 1.05× faster, Linux aarch64 1.08× slower, Linux x86-64 1.01× slower; matmul: macOS arm64 1.21× slower, Linux aarch64 1.04× slower, Linux x86-64 1.07× slower; nbody: macOS arm64 1.31× slower, Linux aarch64 1.04× slower, Linux x86-64 1.02× faster; mandelbrot: macOS arm64 1.01× slower, Linux aarch64 1.00×, Linux x86-64 1.03× faster; words: macOS arm64 1.37× slower, Linux aarch64 1.44× slower, Linux x86-64 1.13× slower; numeric: macOS arm64 1.31× slower, Linux aarch64 1.22× slower, Linux x86-64 1.02× faster; jsonrt: macOS arm64 ≈1.38× slower, Linux aarch64 1.42× slower, Linux x86-64 1.25× slower; ripgrep: macOS arm64 ≈1.50× slower, Linux aarch64 1.30× slower, Linux x86-64 1.21× slower; fd: macOS arm64 ≈1.12× slower, Linux aarch64 1.23× slower, Linux x86-64 1.27× slower; tokei: macOS arm64 1.29× slower, Linux aarch64 1.21× slower, Linux x86-64 1.32× slower; eza: macOS arm64 ≈1.06× slower, Linux aarch64 1.05× slower, Linux x86-64 1.08× slower; bat: macOS arm64 ≈1.16× slower, Linux aarch64 1.13× slower, Linux x86-64 1.17× slower; bun: macOS arm64 1.25× slower, Linux aarch64 1.17× slower, Linux x86-64 1.22× slower" src="benchmarks/charts/runtime-light.svg" width="760"></picture>

How fast the programs run. Each program is built by `cargo build --release` and by blaze with LLVM at `-O3`, every crate of it compiled by blaze, with the release profile; each benchmark is then run several times, the builds taking turns, median. A run is published only when every program did what the release build's did. Each platform is a machine of its own, and each number is blaze's time ÷ release's on it: 1x is as fast.

| Benchmark | macOS arm64 | Linux aarch64 | Linux x86-64 |
|---|---:|---:|---:|
| sieve | 1.12x | 1.07x | 1.00x |
| collatz | 1.01x | 1.00x | 1.00x |
| fnv | 1.00x | 1.01x | 1.00x |
| qsort | 0.95x | 1.08x | 1.01x |
| matmul | 1.21x | 1.04x | 1.07x |
| nbody | 1.31x | 1.04x | 0.98x |
| mandelbrot | 1.01x | 1.00x | 0.97x |
| words | 1.37x | 1.44x | 1.13x |
| numeric | 1.31x | 1.22x | 0.98x |
| jsonrt | ≈1.38x | 1.42x | 1.25x |
| ripgrep | ≈1.50x | 1.30x | 1.21x |
| fd | ≈1.12x | 1.23x | 1.27x |
| tokei | 1.29x | 1.21x | 1.32x |
| eza | ≈1.06x | 1.05x | 1.08x |
| bat | ≈1.16x | 1.13x | 1.17x |
| bun | 1.25x | 1.17x | 1.22x |
| **All benchmarks (geometric mean)** | **1.18x** | **1.14x** | **1.10x** |

≈: that benchmark's runs varied by more than 1.5× from fastest to slowest on that machine; read it as rough.

- **macOS arm64**: GitHub-hosted macos-15 runner (a shared VM) (Apple M1 (Virtual), 3 cores, 7 GiB), blaze [`9c31e64d83`](https://github.com/indexable-inc/blaze/commit/9c31e64d83cd4cc2d0bbbd2e56b7e48d8a9326c6) on 2026-10-23, each benchmark run 5 times, load average 12 at the start, [by CI](https://github.com/indexable-inc/blaze/actions/runs/39876666206).
- **Linux aarch64**: AWS c8g.8xlarge (Graviton4), pinned to 16 cores with taskset (Neoverse-V2, 16 of its 32 cores, 62 GiB), blaze [`e8d8a63da4`](https://github.com/indexable-inc/blaze/commit/e8d8a63da4) on 2026-10-23, each benchmark run 10 times, load average 11 at the start, by hand.
- **Linux x86-64**: AWS c8a.8xlarge, pinned to 16 cores with taskset (AMD EPYC 9R45, 16 of its 32 cores, 62 GiB), blaze [`e8d8a63da4`](https://github.com/indexable-inc/blaze/commit/e8d8a63da4) on 2026-10-23, each benchmark run 10 times, load average 13 at the start, by hand.
- **bun** is measured on its own, by the `bench-bun` workflow: macOS arm64 on one machine (Apple M5 Max, 18 cores, 128 GiB), blaze [`948d54e375`](https://github.com/indexable-inc/blaze/commit/948d54e375ef4f4685a932539d3a4118992a31c1) on 2026-10-23, each benchmark run 5 times, load average 29 at the start, by hand; Linux aarch64 on one machine (Neoverse-V2, 16 of its 96 cores, 185 GiB), blaze [`68c3558698`](https://github.com/indexable-inc/blaze/commit/68c3558698d1a050d728c592737766712fefae86) on 2026-10-23, each benchmark run 5 times, load average 12 at the start, by hand; Linux x86-64 on one machine (AMD EPYC 9R45, 16 of its 96 cores, 185 GiB), blaze [`68c3558698`](https://github.com/indexable-inc/blaze/commit/68c3558698d1a050d728c592737766712fefae86) on 2026-10-23, each benchmark run 5 times, load average 12 at the start, by hand.

<details><summary>Times, memory and builds, by platform</summary>

**macOS arm64**

| Benchmark | `cargo build --release` | blaze, LLVM | blaze ÷ release | release with dev checks ÷ release | Peak memory, release → blaze |
|---|---:|---:|---:|---:|---:|
| sieve | 45 ms | **50 ms** | 1.12x | 1.01x | 17 MB → 17 MB |
| collatz | 150 ms | **151 ms** | 1.01x | 1.49x | 1 MB → 2 MB |
| fnv | 98 ms | **98 ms** | 1.00x | 0.96x | 2 MB → 2 MB |
| qsort | 263 ms | **250 ms** | 0.95x | 0.91x | 13 MB → 13 MB |
| matmul | 32 ms | **39 ms** | 1.21x | 0.99x | 4 MB → 4 MB |
| nbody | 30 ms | **39 ms** | 1.31x | 1.01x | 1 MB → 2 MB |
| mandelbrot | 328 ms | **333 ms** | 1.01x | 1.00x | 1 MB → 2 MB |
| words | 71 ms | **98 ms** | 1.37x | 1.22x | 2 MB → 2 MB |
| numeric | 95 ms | **124 ms** | 1.31x | 1.17x | 14 MB → 14 MB |
| jsonrt | 1.23 s | **1.69 s** | ≈1.38x | 1.14x | 597 MB → 590 MB |
| ripgrep | 134 ms | **201 ms** | ≈1.50x | 1.10x | 5 MB → 7 MB |
| fd | 63 ms | **71 ms** | ≈1.12x | 0.98x | 7 MB → 8 MB |
| tokei | 643 ms | **828 ms** | 1.29x | 1.09x | 26 MB → 27 MB |
| eza | 417 ms | **440 ms** | ≈1.06x | 0.92x | 19 MB → 20 MB |
| bat | 156 ms | **181 ms** | ≈1.16x | 0.89x | 12 MB → 15 MB |
| bun | 332 ms | **415 ms** | 1.25x | - | 43 MB → 46 MB |
| **All benchmarks (geometric mean)** | | | **1.18x** | 1.05x | |

What each program takes, release → blaze: its build from cold (for blaze, cargo running the build scripts and proc macros, blaze's compile of every crate and the link), its size, and its build's memory (the most the build's processes held at once, and the largest one process: rustc or blaze, or the linker).

| Program | Build | Size | Build, peak | Build, largest process |
|---|---:|---:|---:|---:|
| kernels | 600 ms → 6.80 s | 0.48 MB → 1.64 MB | 146 MB → 1.1 GB | 146 MB → 1.1 GB |
| numeric | 3.90 s → 37.80 s | 0.75 MB → 2.23 MB | 288 MB → 1.3 GB | 288 MB → 1.3 GB |
| jsonrt | 11.60 s → 68.60 s | 0.97 MB → 2.63 MB | 570 MB → 1.8 GB | 428 MB → 1.8 GB |
| ripgrep | 33.40 s → 227.10 s | 6.66 MB → 10.92 MB | 553 MB → 2.8 GB | 403 MB → 2.8 GB |
| fd | 49.70 s → 220.30 s | 3.0 MB → 8.77 MB | 757 MB → 2.8 GB | 757 MB → 2.8 GB |
| tokei | 53.30 s → 258.70 s | 4.22 MB → 9.63 MB | 547 MB → 3.2 GB | 429 MB → 3.2 GB |
| eza | 97.50 s → 213.50 s | 2.0 MB → 6.66 MB | 545 MB → 2.9 GB | 545 MB → 2.8 GB |
| bat | 129.70 s → 679.00 s | 6.65 MB → 16.8 MB | 1.2 GB → 4.6 GB | 1.2 GB → 4.6 GB |
| bun | 177.40 s → 983.10 s | 103.09 MB → 129.61 MB | 7.1 GB → 27.3 GB | 4.7 GB → 27.3 GB |

**Linux aarch64**

| Benchmark | `cargo build --release` | blaze, LLVM | blaze ÷ release | release with dev checks ÷ release | Peak memory, release → blaze |
|---|---:|---:|---:|---:|---:|
| sieve | 50 ms | **54 ms** | 1.07x | 1.01x | 23 MB → 23 MB |
| collatz | 105 ms | **105 ms** | 1.00x | 1.91x | 23 MB → 23 MB |
| fnv | 71 ms | **72 ms** | 1.01x | 1.00x | 23 MB → 23 MB |
| qsort | 198 ms | **215 ms** | 1.08x | 1.06x | 23 MB → 23 MB |
| matmul | 28 ms | **29 ms** | 1.04x | 1.00x | 23 MB → 23 MB |
| nbody | 36 ms | **37 ms** | 1.04x | 1.00x | 23 MB → 23 MB |
| mandelbrot | 270 ms | **269 ms** | 1.00x | 1.00x | 23 MB → 23 MB |
| words | 61 ms | **87 ms** | 1.44x | 1.03x | 23 MB → 23 MB |
| numeric | 23 ms | **28 ms** | 1.22x | 1.10x | 41 MB → 41 MB |
| jsonrt | 865 ms | **1.23 s** | 1.42x | 1.05x | 546 MB → 546 MB |
| ripgrep | 58 ms | **75 ms** | 1.30x | 1.03x | 26 MB → 26 MB |
| fd | 18 ms | **23 ms** | 1.23x | 1.01x | 26 MB → 26 MB |
| tokei | 90 ms | **108 ms** | 1.21x | 1.00x | 50 MB → 52 MB |
| eza | 231 ms | **243 ms** | 1.05x | 0.99x | 46 MB → 45 MB |
| bat | 88 ms | **99 ms** | 1.13x | 1.04x | 30 MB → 30 MB |
| bun | 535 ms | **624 ms** | 1.17x | 1.10x | 54 MB → 58 MB |
| **All benchmarks (geometric mean)** | | | **1.14x** | 1.07x | |

What each program takes, release → blaze: its build from cold (for blaze, cargo running the build scripts and proc macros, blaze's compile of every crate and the link), its size, and its build's memory (the most the build's processes held at once, and the largest one process: rustc or blaze, or the linker).

| Program | Build | Size | Build, peak | Build, largest process |
|---|---:|---:|---:|---:|
| kernels | 500 ms → 4.50 s | 0.55 MB → 0.55 MB | 187 MB → 1.5 GB | 169 MB → 1.4 GB |
| numeric | 3.60 s → 13.10 s | 0.88 MB → 0.72 MB | 600 MB → 1.8 GB | 311 MB → 1.8 GB |
| jsonrt | 6.70 s → 21.10 s | 1.09 MB → 1.07 MB | 2.0 GB → 2.8 GB | 441 MB → 2.7 GB |
| ripgrep | 9.70 s → 57.30 s | 32.23 MB → 16.06 MB | 2.4 GB → 3.6 GB | 716 MB → 3.6 GB |
| fd | 49.10 s → 77.60 s | 3.74 MB → 4.34 MB | 2.3 GB → 3.8 GB | 790 MB → 3.8 GB |
| tokei | 14.50 s → 63.30 s | 5.43 MB → 5.55 MB | 3.4 GB → 4.5 GB | 539 MB → 4.5 GB |
| eza | 37.90 s → 45.20 s | 2.38 MB → 3.17 MB | 3.4 GB → 4.0 GB | 573 MB → 3.9 GB |
| bat | 53.70 s → 127.90 s | 6.99 MB → 8.51 MB | 3.7 GB → 6.2 GB | 1.3 GB → 6.2 GB |
| bun | 167.40 s → 160.20 s | 318.66 MB → 301.54 MB | 6.5 GB → 16.0 GB | 3.7 GB → 16.0 GB |

**Linux x86-64**

| Benchmark | `cargo build --release` | blaze, LLVM | blaze ÷ release | release with dev checks ÷ release | Peak memory, release → blaze |
|---|---:|---:|---:|---:|---:|
| sieve | 26 ms | **26 ms** | 1.00x | 1.00x | 22 MB → 22 MB |
| collatz | 94 ms | **94 ms** | 1.00x | 1.80x | 22 MB → 22 MB |
| fnv | 59 ms | **59 ms** | 1.00x | 1.00x | 22 MB → 22 MB |
| qsort | 203 ms | **206 ms** | 1.01x | 1.04x | 22 MB → 22 MB |
| matmul | 22 ms | **24 ms** | 1.07x | 1.01x | 22 MB → 22 MB |
| nbody | 20 ms | **19 ms** | 0.98x | 1.00x | 22 MB → 22 MB |
| mandelbrot | 180 ms | **175 ms** | 0.97x | 1.02x | 22 MB → 22 MB |
| words | 40 ms | **45 ms** | 1.13x | 1.14x | 22 MB → 22 MB |
| numeric | 21 ms | **21 ms** | 0.98x | 1.03x | 22 MB → 22 MB |
| jsonrt | 884 ms | **1.10 s** | 1.25x | 1.06x | 542 MB → 542 MB |
| ripgrep | 31 ms | **37 ms** | 1.21x | 1.06x | 23 MB → 23 MB |
| fd | 12 ms | **14 ms** | 1.27x | 1.02x | 23 MB → 23 MB |
| tokei | 63 ms | **83 ms** | 1.32x | 1.02x | 36 MB → 36 MB |
| eza | 123 ms | **134 ms** | 1.08x | 1.05x | 30 MB → 30 MB |
| bat | 54 ms | **63 ms** | 1.17x | 1.04x | 30 MB → 30 MB |
| bun | 389 ms | **476 ms** | 1.22x | 1.10x | 51 MB → 62 MB |
| **All benchmarks (geometric mean)** | | | **1.10x** | 1.07x | |

What each program takes, release → blaze: its build from cold (for blaze, cargo running the build scripts and proc macros, blaze's compile of every crate and the link), its size, and its build's memory (the most the build's processes held at once, and the largest one process: rustc or blaze, or the linker).

| Program | Build | Size | Build, peak | Build, largest process |
|---|---:|---:|---:|---:|
| kernels | 300 ms → 3.00 s | 0.5 MB → 0.52 MB | 256 MB → 1.5 GB | 195 MB → 1.4 GB |
| numeric | 2.20 s → 9.10 s | 0.81 MB → 0.8 MB | 707 MB → 1.9 GB | 333 MB → 1.8 GB |
| jsonrt | 4.20 s → 14.70 s | 1.07 MB → 1.22 MB | 2.4 GB → 2.9 GB | 460 MB → 2.8 GB |
| ripgrep | 6.00 s → 44.20 s | 31.4 MB → 17.26 MB | 2.7 GB → 3.6 GB | 619 MB → 3.5 GB |
| fd | 34.60 s → 59.30 s | 4.16 MB → 4.85 MB | 2.7 GB → 3.9 GB | 497 MB → 3.8 GB |
| tokei | 9.30 s → 47.10 s | 5.51 MB → 6.49 MB | 3.8 GB → 4.7 GB | 516 MB → 4.6 GB |
| eza | 25.40 s → 32.70 s | 2.68 MB → 3.4 MB | 3.8 GB → 3.9 GB | 509 MB → 3.8 GB |
| bat | 38.70 s → 98.20 s | 7.56 MB → 9.34 MB | 4.1 GB → 6.2 GB | 730 MB → 6.1 GB |
| bun | 135.60 s → 129.20 s | 314.01 MB → 297.55 MB | 6.2 GB → 15.9 GB | 3.7 GB → 15.8 GB |

Release with dev checks: `cargo build --release` with `-C overflow-checks=on -C debug-assertions=on`.

</details>

Measured by hand with `harness/bench.py --runtime` (harness/runbench.py), or by the `bench-readme` workflow (a project measured on its own, by its own workflow) on each platform's benchmark machine once it is registered. Raw JSON: [macOS arm64](benchmarks/runtime-macos-arm64.json), [Linux aarch64](benchmarks/runtime-linux-aarch64.json), [Linux x86-64](benchmarks/runtime-linux-x86-64.json); bun: [macOS arm64](benchmarks/runtime-bun-macos-arm64.json), [Linux aarch64](benchmarks/runtime-bun-linux-aarch64.json), [Linux x86-64](benchmarks/runtime-bun-linux-x86-64.json).
<!-- runbench:end -->

## What is measured

`harness/bench.py` copies each project's pinned checkout and builds it two ways side by side:
`blaze build` (the daemon) and plain `cargo build` into a target directory of its own.
Both build the same source after every edit, one after the other, so they see the same machine
state.

| Phase | What happens | blaze | cargo |
|---|---|---|---|
| Cold | build from an empty target directory and an empty blaze store; crates are already downloaded | the build returns (cargo builds the first time) and then the daemon loads the workspace in the background; both are reported | `cargo build` |
| Cold, warm store | after everything else, `--cold-runs` times: a second copy of the project with an empty target directory and a daemon of its own, but the store the measured copy filled with its cold build and edits (as a second checkout of the same project, or a teammate's store, would have) | build and daemon ready, as cold | |
| Nothing changed | build again with no edit | | |
| Edit a function | one statement is added to a function in the main crate | | incremental |
| Edit a widely used function | one statement in a function many places call: in a library crate of the workspace when there is one (ripgrep's `grep-matcher`, axum's `axum-core`, nushell's `nu-path`, bat's library), otherwise in a module the rest of the crate uses (fd, hyperfine, just) | | incremental |
| Change a signature | a method gets another parameter and every caller passes it | | incremental |
| Edit a generic function | a statement in a generic function used at many types (ripgrep and nushell) | | incremental |
| Add a function | a new function in the main crate, called from the function of "Edit a function" | | incremental |
| Add a struct field | a struct gets one more field (`_bench_edit: ()`) and every place that builds it sets it; in a library crate where there is one (nushell's `nu-protocol::Alias`, built in `nu-parser`) | | incremental |
| Tests | (`--tests`) the project's test suite, or the crates of it named in `PROJECTS`, after the edits: `blaze test` and `cargo test`, run twice each, the second timed; every test has to end the same way under both | | |
| Runtime | (ripgrep) the program blaze linked, cargo's debug build and cargo's release build run the same search over the toolchain's own library source | | |

The edits do not change what the program does. After the first edit of each kind the program
blaze linked is run and has to print what cargo's prints (exit status, stdout and stderr); in
the runtime phase all three programs have to print the same. A run in which any program differs
is not published (`benchreport.py` refuses it and the workflow fails).

Each edit is made and taken back `--runs` times (5 in CI). The summary tables show the median of
the edits; the results below also show the fastest and slowest run of each side, and the raw JSON keeps
every sample, the time of taking each edit back, who built each step (the daemon, or cargo and
why), whether the daemon spliced the change into the program or linked it again, how many
functions it compiled, the memory of both sides (below) and the daemon's own timeline of each
build.
Cold is `--cold-runs` runs of each (5 in CI), each with a new target directory, home and store;
the store size after it is kept too.

An edit the daemon hands to cargo (a fallback: blaze always builds what cargo would) is measured
once, marked `(c)` in the tables, and shows about cargo's time.

## Memory and disk

Every build in the table is also measured for memory, on both sides, and so is the disk each
side leaves behind. [The summary](#how-much-memory-and-disk-it-takes) is at the top of this page;
[the results](#results) have all of it.

| Number | blaze | cargo |
|---|---|---|
| Peak, each phase and edit | the most `blaze build`, the daemon and every process under either held at once: cargo when the client hands it the build, its rustc, build scripts and linker, the daemon's workers and what they run | the most `cargo build`, rustc, build scripts and the linker held at once |
| Largest process | the peak of the largest of those processes on its own | the same |
| Daemon between edits | what the daemon and its workers hold once it is ready, after nothing changed, after each kind of edit and at the end of the run, with the number of builds so far: memory that grows with every edit shows here | |
| On disk | the target directory, the store and blaze's home | the target directory |
| Cold, warm store | the peak, and what the daemon holds once ready | |
| Runtime | the peak of the program blaze linked while it runs | the peak of cargo's debug and release programs |

`harness/runbench.py` (runtime parity) adds the peak of each program while it runs, the size of
each binary, and the peak of each build: `cargo build --release` against blaze's whole-program
LLVM build.

**How.** One helper, `harness/memory.py`, measures all of it, and every harness that reports
memory uses it. It combines two readings:

- **The exact peak of one process.** When the harness waits for a command (`wait4`), the
  kernel reports the largest resident size any single process of what it waited for reached
  (`ru_maxrss`; KiB on Linux, bytes on macOS). This is exact, but it is one process: cargo's 16
  rustc at once count as the largest of them.
- **The sum over a tree, sampled.** A thread reads what every process under the command, and
  under the daemon, holds every 10 ms (`BLAZE_MEMORY_INTERVAL`) and keeps the largest sum. On
  Linux that is the resident size, from `/proc/PID/statm`, with the children of each thread in
  `/proc/PID/task/*/children`. On macOS it is the physical footprint, from
  `proc_pid_rusage`, with `proc_listchildpids`: what Activity Monitor shows, the memory a
  process owns, still counted when macOS has compressed it or swapped it out. Resident size
  would not do on a Mac: when memory is short macOS compresses the pages of a process that is
  idle, and the daemon between two edits read 119 MB on nushell on a Mac whose memory was full,
  where it held 7 GB a minute before. The kernel also keeps each process's peak footprint,
  which the sampler reads as the largest of that process. Each process it saw stays watched,
  so the daemon, which the first build starts and which then outlives it, is followed with its
  workers.

The peak reported is the sum, and never less than the peak of the largest process. What the
daemon holds between edits is one reading of its tree when it is idle.

**Error.** The sum can miss a rise shorter than 10 ms and a process that lives less than
10 ms (a short `cc` or build script): it reads low by at most what such a process held. On
Linux it can also read high: a page of a shared library that several processes have mapped
counts once in each (resident size, not proportional size). That is mostly libLLVM and
`librustc_driver`, so it inflates cargo's peak with many rustc at once more than blaze's,
whose daemon is one process with a few workers; proportional size costs a walk of every
mapping per reading. The footprint macOS counts leaves those pages out. A Linux number and a
macOS number are therefore not the same count; within one machine both sides are.

**Cost.** The sampling thread wakes every 10 ms and reads a few dozen small files or calls;
while it runs, Python switches threads every 0.1 ms instead of 5 ms, so the end of a timed
command is seen within that. On the MacBook of the macOS results, a no-op `cargo build` of blaze itself took a
median 322 ms without it and 319 ms with it (30 runs each, taking turns): no cost the noise
shows. Two runs of the Mac's measurement gave the same peaks within 1%.

**Disk** is the blocks the files use, a file with several hard links once
(`memory.disk_mb`). On Linux each block is counted once however many files share it: a
copy the file system made by sharing the blocks (a reflink, which `std::fs::copy` makes on
XFS and Btrfs, and which blaze's copies of rustc's objects and libraries are) takes no
more disk than the one file, and is counted so, where `du` counts it once per file. The
kernel says which blocks are shared (`FIEMAP`); where it does not, a file is counted as
`du` counts it. On macOS a clone is counted as `du` counts it, once per file.

The history (`rss_mb`) still has the daemon's workers' high-water marks summed, as before; the
history's memory alerts check the new numbers too (below).

Each platform's memory tables are with the rest of its numbers, under [Results](#results).

## Projects

Pinned by tag and commit in `harness/bench.py` (`PROJECTS`); the clone has to be at that commit
or the run stops.

| Project | Tag | Commit | Run to check the output |
|---|---|---|---|
| ripgrep | 15.1.0 | `af60c2de9d85` | `rg -n --no-heading --sort path "fn main" ...` |
| fd | v10.3.0 | `d38148f0aabd` | `fd --extension rs main src` |
| bat | v0.26.0 | `c734087e1d15` | `bat --plain --color=never rustfmt.toml` |
| hyperfine | v1.19.0 | `12fec4209864` | `hyperfine --help` |
| just | 1.43.0 | `b7e7c46495b3` | `just --list --unsorted` |
| axum | axum-v0.8.6 | `c1bb9c3e9062` | built (`-p example-hello-world`), not run: it is a server |
| nushell | 0.108.0 | `da141be11b3a` | `nu -n -c "[3 1 2] \| sort \| str join ','"` |
| helix | 25.07.1 | `a05c151bb6e8` | `hx --version` (grammars are not built: `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1`) |
| bevy | v0.20.0 | `b3bdd75becaf` | built (`--example breakout`), not run: it opens a window |
| biome | @biomejs/biome@2.6.3 | `c5c3383d590d` | `biome check --reporter=github --colors=off packages/@biomejs/js-api/src/common.ts` (`-p biome_cli --bin biome`) |
| zed | v1.24.4 | `aed37547d854` | `zed --help`; not in the CI run, see below |

zed (1.7 million lines in 249 crates, 1,291 crates with its dependencies) is in `PROJECTS` and
every edit of it builds, but it is not measured yet: blaze's daemon does not fit it in 123 GB.
On a c8i.16xlarge (2026-10-21, blaze `2b2f2dcd5b`) the daemon loaded the workspace (20 s),
its 1,756,350 symbols (26 s) and resolved the crates (25 s), then went from 20 GB to more
than 111 GB in 40 seconds and was stopped at a 112 GB limit before it was ready; cargo alone
builds it in a few minutes on the same machine. Its build needs `nix develop .#bench-zed`.

`cargo` 0.92.0 is in `PROJECTS` too but not in the CI run (it doubles the run's length).

## Fairness

- Same machine, same source, same moment: the two builds alternate after every edit.
- Same toolchain on both sides: the pinned nightly (`harness/toolchain`, `nightly-2026-10-25`;
  the runs dated before 2026-10-25 were on Rust 1.99.0), from the flake's
  `devShells.bench`, which also pins LLVM 23 for the LLVM tier.
- Cargo is warm and incremental: the dev profile's default (`CARGO_INCREMENTAL` unset).
- Same linker: the toolchain's default (`cc`, which runs GNU ld on the benchmark machine). No mold,
  no lld, no sccache or other `RUSTC_WRAPPER` on either side (bench.py removes it).
- Crates are downloaded before the cold build on both sides; downloading is not timed.
- blaze's store starts empty for each project and backend and for every cold run. "Cold, warm
  store" is the one phase that starts with a store, and it is reported apart. On a machine
  the store is one for every project, so the Store column is what a project adds to a store
  that has nothing of it. `store_added_mb` in the results is what it adds to one that has
  what the projects before it in the run stored: the disk of its entries whose keys none of
  them had (a pack's size shared among its entries by the length of their values).
- The machine runs nothing else: it is a dedicated instance used only by this workflow.

## Tiers

Each project is measured with each code producer blaze has: Cranelift (the default, in the
summary tables) and LLVM (`BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`). Cranelift is measured
with blaze as it is installed and released, without the `llvm` feature (`BLAZE_BIN`), and LLVM
with a build that has it (`BLAZE_LLVM_BIN`): a binary that links libLLVM loads it each time it
starts, about 6 ms on the benchmark machine, and every `blaze` starts it once. A new tier
is one more entry in `BACKENDS` in `harness/bench.py`; the report makes a table for every
backend in the result.

## The fastest setups

With `--setups` (all of them in the nightly run, `--setups-on ripgrep,nushell`), every step plain
cargo builds is built again by each of the fastest ways to build Rust there already are, each
into a target directory of its own, right after plain cargo, on the same source:

| Setup | What it is |
|---|---|
| mold | cargo with mold 3.0 as the linker (`-C link-arg=-fuse-ld=mold`) |
| wild | cargo with wild 0.10 as the linker (`-C link-arg=-fuse-ld=wild`) |
| `-Zthreads=8` | rustc's parallel front end, on the same toolchain (on 1.99.0 with `RUSTC_BOOTSTRAP=1` in the runs before 2026-10-25) |
| nightly | plain cargo on the nightly below, to tell the nightly's own difference from Cranelift's; since 2026-10-25 it is the same nightly as plain cargo's, so the two should agree |
| cg_clif | rustc_codegen_cranelift (`-Zcodegen-backend=cranelift`), on the nightly: it ships with nightlies only |
| cg_clif + wild + `-Zthreads=8` | all three at once, the fastest cargo build there is |
| `cargo check` | builds no code, so there is nothing to run after it: the floor of what rustc does |

The flags go in through `--config target.'cfg(all())'.rustflags`, which cargo adds to each
project's own (helix sets `--cfg tokio_unstable` there; `RUSTFLAGS` would replace it).
`-Zshare-generics` is not a column: it is already on in every debug build. The nightly is
`harness/toolchain`'s, `rust-bin.nightly.<date>` with `rustc-codegen-cranelift-preview`, from
the flake (`BLAZE_NIGHTLY`; `rust-bin.nightly."2026-10-09"` in the runs before 2026-10-25). A setup that cannot build a project is reported as failed and not timed
after that.

Hot-patching a running program is compared on its own (`harness/hotpatch.py`): a small program
that calls one function every millisecond through `subsecond::call`, edited 30 times by each of
Dioxus's `dx serve --hotpatch` (subsecond, which patches the running program on Linux),
`blaze build` and incremental `cargo build`, the last two followed by starting the program
again (blaze patches a running program on macOS only). Each edit is timed from writing the file
to the program running the new code. It is a small program on purpose: subsecond needs the
program to call through `subsecond::call` and to keep running, which a command-line tool like
ripgrep does not. The restart flows stop the program before building, as `cargo watch` does.

On Linux (dev-aws-7, 16 cores of a c7i.16xlarge, the bench shell), 30 edits each, milliseconds
from the save to the new code running ([raw JSON](benchmarks/hotpatch/linux-x86_64.json)):

| Tool | median | p90 | min | max | of which the build |
|---|---:|---:|---:|---:|---:|
| subsecond (`dx serve --hotpatch`), patched, 0 restarts | 289 | 463 | 283 | 535 | |
| `blaze build`, then a restart (all 30 built by the daemon and spliced) | **40** | 72 | 39 | 78 | 39 |
| `cargo build`, then a restart | 361 | 413 | 353 | 427 | 360 |

On a program this small, starting it again is nearly free, so blaze's build plus a restart
beats subsecond's patch of the running program; a program with state to rebuild after a
restart would lose that. On a MacBook Pro (M5 Max, 18 cores, on AC, other work running: a load
of 7 to 28), the same 30 edits each ([raw JSON](benchmarks/hotpatch/macbook.json)):

| Tool | median | p90 | min | max | of which the build |
|---|---:|---:|---:|---:|---:|
| subsecond (`dx serve --hotpatch`), patched, 0 restarts | 346 | 417 | 320 | 421 | |
| `blaze build`, then a restart (all 30 built by the daemon) | 469 | 527 | 410 | 841 | 299 |
| `cargo build`, then a restart | 520 | 593 | 486 | 651 | 365 |

On macOS blaze also patches a running program without a restart: a program that links
`blaze-live` (`harness/live`, as small as this one) takes an edit in a median of 12.5 ms (p90
28.0, p99 48.9) when it goes through `blaze build`, and 4.0 ms (p90 11.6, p99 13.8) when an
editor sends it over the daemon's socket (`harness/live_latency.py --program probe cargo socket`,
30 edits each, same MacBook, [raw JSON](benchmarks/live/macbook-probe-cargo.json)). That is the
flow to set against subsecond's; the restart flow above is what blaze does where it cannot
patch, today Linux.

## The machines

Every result records its machine and platform: the name it was given (`BLAZE_BENCH_MACHINE`),
the CPU, the cores the run could use (it may be pinned with `taskset`) and the machine's total,
memory, OS, kernel, rustc, C compiler, linker, LLVM version and the load average when it
started (below and in the JSON). The platforms, their runners' labels and how a result is
filed are `harness/benchplatforms.py`.

Each platform is meant to run on a dedicated machine that runs nothing else, registered as a
self-hosted GitHub Actions runner. A shared runner is no use: GitHub's hosted macOS arm64 runner
(`macos-15`) is a 3-core VM with 7 GB of memory, less than nushell's daemon holds, with
neighbours that move the numbers. The three:

| Platform | Runner labels | Machine | Why |
|---|---|---|---|
| Linux x86-64 | `self-hosted, linux, x64, bench-x86` | `dev-aws-bench-x86` (i-01666717f0de39201, us-west-2a), resized to m8a.4xlarge: 16 AMD EPYC 9R45 (Zen 5) cores, one thread per core, 64 GiB | already launched for it, on demand, ttl a year, tagged `ix-dev-purpose=blaze-bench`; launched as a c8a.4xlarge (32 GiB), too little for nushell's daemon (15 GB) beside cargo |
| Linux aarch64 | `self-hosted, linux, ARM64, bench-arm64` | a new `dev-aws-bench-arm64`, m8g.4xlarge: 16 Graviton4 (Neoverse V2) cores, 64 GiB, on demand | the same core count and memory as the x86 machine; a cloud VM is not bare metal, but its cores are not shared with other tenants |
| macOS arm64 | `self-hosted, macOS, ARM64, bench-macos` | a Mac mini M4 Pro (14 cores, 64 GB) kept for it, plugged in, not sleeping (`sudo pmset -a sleep 0`), or an EC2 mac-m4pro.metal host if it has to be in AWS | bare metal, and the only way to measure macOS: a MacBook in use is busy with other work |

`harness/benchrunner.sh PLATFORM MACHINE` sets a runner up: it checks the platform is the
machine's, installs the runner in `~/actions-runner`, registers it with the platform's labels and
runs it at boot (on Linux as user `andrew` from a user systemd unit,
`blaze-bench-runner.service`, lingering; on macOS as the runner's launchd service), and its
`.env` names the machine for the results. It needs Nix with flakes on the machine:

```
scp harness/benchrunner.sh HOST:
gh api -X POST repos/indexable-inc/blaze/actions/runners/registration-token --jq .token \
  | ssh HOST bash benchrunner.sh linux-aarch64 "'AWS m8g.4xlarge (dev-aws-bench-arm64)'"
```

Then name the platform in the repository variable `BENCH_RUNNERS`, comma-separated, which turns
its jobs on: `gh variable set BENCH_RUNNERS -R indexable-inc/blaze -b linux-x86-64,linux-aarch64,macos-arm64`.
A platform it does not name is skipped by every run rather than left waiting for a runner that
is not there, and the tables keep its last numbers, under their machine, date and commit.
(`BENCH_RUNNER=true`, from before there were platforms, still means Linux x86-64.)

Until the runners exist the numbers are measured by hand with the same harness, pinned to 16
cores with `taskset` so that cargo and the daemon have the 16 cores of the machines above. The
edits: on 2026-10-22, Linux x86-64 on `dev-aws-8` (a c7a.48xlarge, AMD EPYC 9R14, idle and
claimed for the run) and Linux aarch64 on a c8g.16xlarge (Graviton4, Neoverse-V2) launched for
it; macOS arm64's are the MacBook Pro (M5 Max) run of 2026-10-21, made while it was busy with
other work (its load average is under the tables), so its times are noisier than the others'. The
runtime, each benchmark run 10 times on 2026-10-23: Linux x86-64 on `dev-aws-11` (a bare-metal
c8a.metal-24xl, AMD EPYC 9R45, idle and claimed for the run) and Linux aarch64 on a c8g.8xlarge
(Graviton4) launched for it. macOS arm64's runtime is one run on GitHub's `macos-15` runner, a
shared virtual M1 with 3 cores and 7 GiB, whose runs of a benchmark have varied by up to 17×
(eza, which reads the disk), so the runtime table marks every benchmark whose runs varied by more than
1.5× with ≈. The first published run (2026-10-20) was on `dev-aws-11` too.

## When it runs and what it commits

`.github/workflows/bench-readme.yaml` runs on every push to main that touches code (not docs,
the site or the README), every night at 09:17 UTC, and by hand (`gh workflow run bench-readme
--repo indexable-inc/blaze`, with `-f force=true` to write the tables even when nothing moved).
One run at a time; a push during a run waits, and a newer push replaces the waiting one. A run
has a job per registered platform, on its runner, at the same time, and then one job that
publishes them all.

That job hands each platform's result to `harness/benchreport.py`, which compares it with the
stored one of the same platform (`docs/benchmarks/<platform>.json`, e.g. `linux-x86-64.json`;
`runtime-<platform>.json` for the runtime). It replaces the stored result, and writes the
README's numbers and this page's tables from the newest stored result of every platform, only
when:

- a median of either side, in any phase, moved by more than 10% and more than 50 ms,
- a step the daemon built is now built by cargo, or the other way round,
- a peak of either side moved by more than 10% and 100 MB,
- a project or backend was added or removed, or
- the stored run is more than 24 hours old (so the commit the tables name stays within a day
  of main).

It then commits those files to main as `github-actions[bot]` with `[skip ci]` in the message (so
the commit starts no other workflow), once for every platform, rebasing onto main if main moved
during the run. It pushes with the secret `BLAZE_BOT_TOKEN`, a repository admin's token, which
the ruleset on main lets past its required check; the job's own `GITHUB_TOKEN` is refused
([harness.md](harness.md#ci)). `bench-bun.yaml` does the same for bun, nightly, on the same runners: bun takes
hours, so it has a workflow and a stored result of its own (`docs/benchmarks/bun-<platform>.json`
and `runtime-bun-<platform>.json`, written by `benchreport.py --name bun`), but it is a project like
the others in the tables and charts and in their geometric means. Each table folds a
platform's bun result into that platform's own (`benchreport.py`'s `stored_all`), whichever
workflow wrote last, and a footnote names bun's machine, commit and date; its every number is
under "macOS arm64, bun" and so on below.

## History and regressions

Every run of the workflow appends one line per platform to
`docs/benchmarks/history/<platform>.jsonl` (`harness/benchhistory.py build`), whether or not the
tables changed: the medians, spreads, who built each step, memory and store size of each project
and phase, the platform, the machine and the run's link. The site's
[/bench](https://ix.dev/blaze/bench) page charts those files, a platform and machine at a time,
and the workflow starts the site's deploy after it commits.

`benchhistory.py check` then compares the new line with the median of the last seven from the
same machine with the same cores. A number regressed when it is above that median by more than
its noise band and by more than a floor: the band is 10% or three times the spread the history
shows (1.4826 times the median absolute deviation), whichever is larger, and the floors are
50 ms for times, 100 MB for memory and 50 MB for the store and disk. Memory checked is
blaze's: its peak in each phase, what the daemon holds after each phase, once ready and at the
end of the run, and its disk; cargo's memory is charted, not checked. A step the daemon built that cargo
builds now is a regression whatever its time. On a regression the workflow opens an issue
labelled `bench-regression` with a table of what moved (or comments on the one already open),
writes the table to the run's summary and fails.

Live edits are checked the same way. `.github/workflows/live-latency.yaml` runs every night on
main on a GitHub macOS arm64 runner (`macos-15`), times socket, burst and replay edits of the
four live programs (`harness/live_latency.py`, [live.md](live.md#latency-under-typing)), appends
one line for all of them to `docs/benchmarks/history/macos-arm64-live.jsonl` and checks the p50
and p90 of each program and mode (floor 1 ms). A shared runner is noisier than the benchmark
machine; the band widens with the spread its own history shows.

## Run it yourself

On any of the platforms with Nix (or with the pinned nightly of `harness/toolchain`, the pinned LLVM 23 and Python 3 installed; ripgrep's run searches blaze's `library/`, not a rust-src):

```
nix develop .#bench
cargo build -p blaze-engine --release
CARGO_TARGET_DIR=target-llvm cargo build -p blaze-engine --release --features llvm
python3 harness/bench.py ripgrep fd --runs 5 --out /tmp/bench.json   # any of the projects
python3 harness/benchreport.py /tmp/bench.json --check               # what would change
```

`--backend cranelift` measures one tier. `--cold-runs 5` repeats the cold builds, `--setups
all --setups-on ripgrep,nushell` adds the setups above, `--tests` the test suites, and
`--reference DIR` puts cargo's target directories outside the work directory.
`python3 harness/hotpatch.py --edits 30` times hot-patching (needs `dx`: the bench shell has it). Projects are cloned once into `BLAZE_PROJECTS`
(default `~/blaze-projects`). The whole CI set (seven projects, both tiers, five runs) took 38
minutes on 16 Zen 5 cores, plus the build of blaze. It needs about 20 GB of memory (nushell)
and 30 GB of disk.
`python3 harness/bench.py --compare OLD.json NEW.json` lists what got slower or holds more
memory. On macOS give `--work` a short directory (`--work /tmp/bench`): the daemon's socket
path has to fit in 104 bytes, which a work directory under `$TMPDIR` does not leave room for.
`python3 harness/benchreport.py RESULT` publishes a run as its platform's (the machine it ran
on): `BLAZE_BENCH_MACHINE` names that machine in the tables, and `taskset -c 0-15` before
`python3` gives it the 16 cores of the benchmark machines. `benchreport.py --render` writes
every table and chart again from the stored results.

## Caveats

- Live edits (the patch applied to a running program) work on macOS on Apple silicon only and are
  measured there by `harness/live_latency.py`, per keystroke under typing as well as one edit at
  a time: [live.md](live.md#latency-under-typing) has the numbers from a MacBook, and
  `history/macos-arm64-live.jsonl` the runs.
- One machine, one OS. The numbers on a laptop will differ; the ratios are what carries over.
- Cold is the least precise number here: five runs at most, each minutes long on the large
  projects.
- The setups compared are measured on ripgrep and nushell only (`--setups-on`): seven more
  builds of every step on every project would make the nightly run too long for one machine.
- An edit is a small, fixed change. A larger change (many functions, a new type) costs more;
  `harness/agents.py` makes random edits of the kinds coding agents make.
- axum's output is not checked (it is a server); its build is.
- ripgrep's runtime is one short workload (tens of milliseconds, over 16 threads), so it says more
  about start-up and directory walking than about generated code; the program blaze linked has
  had the edits spliced into it and taken back, as a program in use would.
- Cold for `blaze build` is cargo's own build (the daemon is not loaded yet), so it matches
  plain cargo; the daemon then loads the workspace in the background ("Daemon ready after").
- The C compiler comes from Nix (gcc 16) with the wrapper's hardening turned off, as a
  distribution's gcc behaves; it is the same for both sides. Nix's linker wrapper runs
  `realpath` for each library directory of the shell on every link cargo makes (blaze's links
  do not go through it), so the bench shell holds only what the measured projects need, 72
  directories (a link through `cc` costs 0.23 s); zed's libraries are in `nix develop
  .#bench-zed`. With them in the bench shell (282 directories) every cargo link cost 0.85 s, which
  made cargo and subsecond look 0.6 s slower. A result records the count (`library_dirs`).

## What the 2026-10-21 run found

The results below come from two runs on dev-aws-7 (16 cores of a c7i.16xlarge, blaze
`2b2f2dcd5b`): ripgrep to nushell on both tiers in one, bevy and helix on Cranelift with one
cold run each in the second, since helix with its test targets needs more memory than the
machine has. blaze loses or fails on these:

- **Doctests.** Without `-q`, `blaze test` builds the doctests of every library itself, in
  a process that loads the workspace each time (about 2 s per library), on every run: ripgrep's
  suite takes 27.8 s with nothing changed against 4.3 s under `cargo test`, nushell's 12.4 s
  against 2.5 s. With `-q` cargo hands the doctests to rustdoc and ripgrep's run takes 4.9 s
  against 4.1 s. fd, hyperfine and just, whose libraries have few or no doctests, are faster than
  cargo after an edit and the same with nothing changed. In ripgrep, nushell and axum the edited
  library is one whose unit tests the daemon leaves to rustc, so that edit is cargo's build.
- **axum's `example-testing`.** The test binary the daemon builds crashes (SIGSEGV) in
  `the_real_deal`, which serves axum on a TCP socket and calls it with hyper's client: tokio's
  `Registration::readiness` reads a null `Arc<ScheduledIo>` from `TcpListener::accept`. Five of
  the six tests pass; cargo's binary passes all six. Seen on Linux and on macOS.
- **bevy's breakout.** The daemon cannot type `(apply_velocity, move_paddle,
  check_for_collisions).chain()` (the method comes from impls bevy generates for tuples of
  systems), and `blaze build` fails at the first edit instead of handing the build to
  cargo. Cold and no-op are measured; the edits are not.
- **helix.** After a 31 s cold build (the same as cargo's) the daemon takes 638 s to be ready
  (636 to 641 s over five cold runs) and holds 98 GB; with a warm store it is no faster (681 s).
  With the test targets loaded too it was killed for memory on a 123 GB machine. Its edits, once
  ready, are 10x to 48x faster than cargo's.
  nushell, a larger workspace, is ready 33 s after its build, at 16 GB.
- **zed** (1.7M lines, 1,291 crates) does not fit: the daemon needs more than 111 GB.
- **Cold.** `blaze build` from nothing is cargo's build plus the start of the daemon:
  6.29 s against 5.56 s on ripgrep, and the daemon is ready 6.2 s later.
- **No-op** on ripgrep: 33 ms against cargo's 30 ms. This run's Cranelift column used the
  binary built with the `llvm` feature, which spends about 6 ms loading libLLVM at every start;
  the bench now measures that column with blaze as released (19 ms on ripgrep in
  [results.md](results.md#warm-edits-on-linux-x86-64-where-the-time-goes-and-build-scripts-2026-10-20)).

Two commits after `2b2f2dcd5b` (`20708cd2da`, `c8f58ac7d9`) stop the daemon from running build
scripts again that cargo just ran, which roughly halves the edits of fd and hyperfine (fd's
function edit 88 to 43 ms on another machine, same page). The next run of the workflow
measures them here.

## Earlier one-off measurements

Edit, then build, on a workspace of 200 generated crates (`harness/genws.py`), Linux x86-64,
8 CPUs, both warm and incremental, same linker, median of three. Measured once by hand before
this page existed; not updated by CI.

| Edit | cargo | blaze | |
|---|---:|---:|---:|
| A function in the main crate | 1.76 s | 0.42 s | 4.2x |
| A function in a crate everything depends on | 6.33 s | 0.60 s | 10.6x |
| Functions in 30 crates at once | 6.26 s | 0.62 s | 10.1x |
| A new function | 2.09 s | 0.56 s | 3.7x |
| A signature used in every crate | 13.87 s | 10.87 s | 1.3x |

## Results

<!-- bench:start -->
Every number of the newest run of each platform, then of each project measured on its own on it (bun, by the `bench-bun` workflow), under its own machine. Each platform is a machine of its own: its numbers compare with each other and with its own history, not with another platform's.

### macOS arm64

blaze [`84760c3f51`](https://github.com/indexable-inc/blaze/commit/84760c3f517a76f6e8a3aa41ccd81fc3f26b87c7), 2026-10-21T21:03:27Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/macos-arm64.json](benchmarks/macos-arm64.json).

| Machine | |
|---|---|
| Name | MacBook Pro (Mac17,6), macOS 27.0.1, with other work running |
| CPU | Apple M5 Max |
| Cores | 18 |
| Memory (GiB) | 128 |
| OS | macOS |
| Kernel | 27.0.0 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | Apple clang version 21.0.0 (clang-2100.3.34.2) |
| Linker | - |
| LLVM | - |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 48.0, 46.1, 48.4 |

#### Edits, Cranelift, macOS arm64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 993 ms | 126 ms | 7.9x | 646 ms to 1.11 s | 125 ms to 191 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 1.31 s | 159 ms | 8.2x | 1.02 s to 4.28 s | 104 ms to 160 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 828 ms | 175 ms | 4.7x | 785 ms to 1.36 s | 102 ms to 305 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 1.93 s | 100 ms | 19x | 1.27 s to 1.95 s | 92 ms to 157 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 1.81 s | 248 ms | 7.3x | 706 ms to 2.86 s | 104 ms to 271 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 1.88 s | 191 ms | 9.8x | 1.35 s to 1.96 s | 94 ms to 298 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 67 ms | 54 ms | 1.2x | 63 ms to 72 ms | 50 ms to 61 ms | daemon |  |
| fd 10.3.0 | Edit a function | 630 ms | 109 ms | 5.8x | 611 ms to 1.29 s | 99 ms to 140 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 734 ms | 102 ms | 7.2x | 691 ms to 1.01 s | 87 ms to 105 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 681 ms | 113 ms | 6.0x | 563 ms to 835 ms | 100 ms to 205 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 712 ms | 126 ms | 5.7x | 593 ms to 745 ms | 106 ms to 228 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 1.09 s | 127 ms | 8.6x | 894 ms to 1.31 s | 113 ms to 143 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 144 ms | 49 ms | 2.9x | 85 ms to 369 ms | 48 ms to 84 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.81 s | 454 ms | 4.0x | 1.66 s to 1.89 s | 383 ms to 713 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 14.39 s | 850 ms | 17x | 12.49 s to 15.24 s | 840 ms to 861 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 10.28 s | 716 ms | 14x | 9.54 s to 12.34 s | 694 ms to 867 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 20.61 s | 2.12 s | 9.7x | 9.59 s to 26.96 s | 986 ms to 2.50 s | daemon | full link: a program too large for frames in front of its own: one is 0x1000020 bytes in |
| nushell 0.108.0 | Add a function | 2.47 s | 444 ms | 5.6x | 1.74 s to 2.86 s | 353 ms to 480 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 25.84 s | 2.51 s | 10x | 23.58 s to 30.94 s | 2.21 s to 3.54 s | daemon | full link: a program too large for frames in front of its own: one is 0x1000008 bytes in |
| nushell 0.108.0 | Nothing changed | 407 ms | 45 ms | 9.0x | 347 ms to 433 ms | 41 ms to 56 ms | daemon |  |

#### Cold, Cranelift, macOS arm64

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 9.83 s | 12.08 s | 14.68 s | 17.62 s | 18,437 | 2,360 MB | 72 MB |
| fd 10.3.0 | 9.17 s | 11.57 s | 12.62 s | 13.16 s | 8,478 | 1,946 MB | 32 MB |
| nushell 0.108.0 | 49.69 s | 77.23 s | 92.67 s | 139.83 s | 167,893 | 16,137 MB | 1,121 MB |

#### Memory and disk, Cranelift, macOS arm64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 541 MB | 2.4 GB | 356 MB | 2.2 GB | 2.2 GB |
| ripgrep 15.1.0 | Edit a function | 235 MB | 2.9 GB | 235 MB | 1.9 GB | 2.9 GB |
| ripgrep 15.1.0 | Edit a widely used function | 243 MB | 2.9 GB | 243 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Change a signature | 235 MB | 2.9 GB | 235 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Edit a generic function | 237 MB | 2.9 GB | 237 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Add a function | 234 MB | 2.9 GB | 234 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Add a struct field | 286 MB | 2.9 GB | 286 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Nothing changed | 40 MB | 2.8 GB | 40 MB | 1.9 GB | 2.8 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 897 MB | 1.9 GB | 389 MB | 1.8 GB | 1.8 GB |
| fd 10.3.0 | Edit a function | 212 MB | 2.6 GB | 212 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Edit a widely used function | 199 MB | 2.6 GB | 192 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Change a signature | 226 MB | 2.6 GB | 226 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Add a function | 211 MB | 2.6 GB | 211 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Add a struct field | 258 MB | 2.6 GB | 258 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Nothing changed | 48 MB | 2.5 GB | 48 MB | 1.6 GB | 2.5 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 2.4 GB | 16.1 GB | 1.1 GB | 15.3 GB | 11.5 GB |
| nushell 0.108.0 | Edit a function | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Edit a widely used function | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Change a signature | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Edit a generic function | 1.1 GB | 13.2 GB | 1.1 GB | 11.3 GB | 12.2 GB |
| nushell 0.108.0 | Add a function | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Add a struct field | 1.1 GB | 13.3 GB | 1.1 GB | 11.4 GB | 12.2 GB |
| nushell 0.108.0 | Nothing changed | 156 MB | 12.1 GB | 156 MB | 11.2 GB | 12.1 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 2.2 GB | 2.8 GB | 2.8 GB after 39 builds | +23 MB | 2.3 GB, 2.0 GB | 416 MB | 526 MB | 72 MB | 0 MB |
| fd 10.3.0 | 1.8 GB | 2.5 GB | 2.6 GB after 33 builds | +50 MB | 1.9 GB, 1.6 GB | 355 MB | 427 MB | 33 MB | 0 MB |
| nushell 0.108.0 | 11.5 GB | 12.1 GB | 12.5 GB after 39 builds | +367 MB | 16.5 GB, 11.7 GB | 5.3 GB | 5.8 GB | 1.1 GB | 1 MB |

#### How fast the program runs, Cranelift, macOS arm64

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.01 s (676 ms to 1.43 s); 12 MB, 28.5 MB | 753 ms (679 ms to 1.07 s); 8 MB, 15.6 MB | 167 ms (129 ms to 308 ms); 6 MB, 6.6 MB |

### macOS arm64, bun

blaze [`27077156cc`](https://github.com/indexable-inc/blaze/commit/27077156ccbd476079d261e529fc25b3225f0ace), 2026-10-23T05:22:33Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/bun-macos-arm64.json](benchmarks/bun-macos-arm64.json).

| Machine | |
|---|---|
| Name | - |
| CPU | Apple M5 Max |
| Cores | 18 |
| Memory (GiB) | 128 |
| OS | macOS |
| Kernel | 27.0.0 |
| rustc | rustc 1.100.0-nightly (574ff7d98 2026-09-14) |
| C compiler | clang version 21.1.8 |
| Linker | - |
| LLVM | 23.1.3 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 8.5, 10.0, 13.9 |

#### Edits, Cranelift, macOS arm64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 7.55 s | 362 ms | 21x | 6.96 s to 7.71 s | 305 ms to 1.07 s | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 16.01 s | 457 ms | 35x | 15.53 s to 16.95 s | 454 ms to 604 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 7.26 s | 823 ms | 8.8x | 6.30 s to 8.27 s | 566 ms to 573.75 s | daemon | full link: no executable to add to |
| bun 1.4.3 | Edit a generic function | 14.91 s | 568 ms | 26x | 13.09 s to 15.59 s | 523 ms to 692 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 11.10 s | 558 ms | 20x | 10.77 s to 11.71 s | 557 ms to 566 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 199 ms | 122 ms | 1.6x | 137 ms to 214 ms | 90 ms to 190 ms | daemon |  |

#### Cold, Cranelift, macOS arm64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 83.29 s | 79.49 s | 81.44 s | 64.31 s | 0 | 13,684 MB | 2,654 MB |

#### Memory and disk, Cranelift, macOS arm64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 3.1 GB | 13.7 GB | 2.7 GB | 11.8 GB | 11.8 GB |
| bun 1.4.3 | Edit a function | 2.1 GB | 26.1 GB | 2.1 GB | 26.0 GB | 26.1 GB |
| bun 1.4.3 | Edit a widely used function | 2.1 GB | 27.7 GB | 2.1 GB | 27.7 GB | 27.7 GB |
| bun 1.4.3 | Change a signature | 2.1 GB | 27.7 GB | 2.1 GB | 27.7 GB | 27.7 GB |
| bun 1.4.3 | Edit a generic function | 2.3 GB | 27.7 GB | 2.3 GB | 27.7 GB | 27.7 GB |
| bun 1.4.3 | Add a function | 2.2 GB | 27.8 GB | 2.2 GB | 27.7 GB | 27.8 GB |
| bun 1.4.3 | Nothing changed | 98 MB | 26.1 GB | 98 MB | 26.0 GB | 26.0 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 11.8 GB | 26.0 GB | 27.8 GB after 33 builds | +1.7 GB | 13.8 GB, 12.0 GB | 6.0 GB | 8.6 GB | 2.6 GB | 103 MB |

#### How fast the program runs, Cranelift, macOS arm64, bun

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 1.33 s (1.08 s to 1.55 s); 74 MB, 478.6 MB | 846 ms (720 ms to 935 ms); 54 MB, 245.9 MB | 113 ms (108 ms to 151 ms); 41 MB, 103.1 MB |

### Linux aarch64

blaze [`c93e530e47`](https://github.com/indexable-inc/blaze/commit/c93e530e474c4de2d67a7a9ec49fd5581894efb8), 2026-10-23T21:59:50Z, Rust 1.99.0, 5 runs of each edit, measured by hand. Raw: [benchmarks/linux-aarch64.json](benchmarks/linux-aarch64.json).

| Machine | |
|---|---|
| Name | AWS c8g.16xlarge, pinned to 16 cores with taskset |
| CPU | Neoverse-V2 |
| Cores | 16 |
| Memory (GiB) | 124 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 20.1.8 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 7.6, 3.2, 1.2 |

#### Edits, Cranelift, Linux aarch64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 1.18 s | 62 ms | 19x | 1.17 s to 1.19 s | 62 ms to 75 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 1.54 s | 57 ms | 27x | 1.53 s to 1.57 s | 57 ms to 94 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 1.34 s | 101 ms | 13x | 1.34 s to 1.35 s | 101 ms to 141 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 1.51 s | 55 ms | 27x | 1.50 s to 1.52 s | 55 ms to 55 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 1.20 s | 69 ms | 17x | 1.20 s to 1.20 s | 68 ms to 71 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 1.72 s | 64 ms | 27x | 1.72 s to 1.75 s | 64 ms to 79 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 25 ms | 18 ms | 1.4x | 24 ms to 25 ms | 18 ms to 21 ms | daemon |  |
| fd 10.3.0 | Edit a function | 1.27 s | 58 ms | 22x | 1.27 s to 1.29 s | 57 ms to 135 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 1.24 s | 35 ms | 35x | 1.23 s to 1.26 s | 35 ms to 47 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 1.29 s | 64 ms | 20x | 1.28 s to 1.30 s | 63 ms to 176 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 1.28 s | 62 ms | 21x | 1.27 s to 1.29 s | 61 ms to 98 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 1.53 s | 51 ms | 30x | 1.52 s to 1.57 s | 51 ms to 83 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 34 ms | 17 ms | 2.0x | 34 ms to 34 ms | 17 ms to 18 ms | daemon |  |
| bat 0.26.0 | Edit a function | 1.62 s | 56 ms | 29x | 1.61 s to 1.63 s | 55 ms to 76 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 1.92 s | 55 ms | 35x | 1.91 s to 1.92 s | 55 ms to 56 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 1.87 s | 57 ms | 33x | 1.87 s to 1.88 s | 57 ms to 89 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 1.62 s | 62 ms | 26x | 1.62 s to 1.62 s | 60 ms to 65 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 2.22 s | 58 ms | 38x | 2.21 s to 2.26 s | 57 ms to 62 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 65 ms | 17 ms | 3.8x | 65 ms to 65 ms | 17 ms to 18 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 753 ms | 38 ms | 20x | 743 ms to 769 ms | 38 ms to 45 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 797 ms | 59 ms | 14x | 793 ms to 799 ms | 59 ms to 61 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 806 ms | 93 ms | 8.7x | 803 ms to 808 ms | 93 ms to 101 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 751 ms | 41 ms | 18x | 746 ms to 753 ms | 41 ms to 42 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 886 ms | 78 ms | 11x | 879 ms to 903 ms | 77 ms to 79 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 44 ms | 17 ms | 2.6x | 44 ms to 44 ms | 17 ms to 18 ms | daemon |  |
| just 1.43.0 | Edit a function | 1.51 s | 43 ms | 35x | 1.49 s to 1.52 s | 42 ms to 51 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 1.53 s | 71 ms | 22x | 1.51 s to 1.54 s | 70 ms to 73 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 1.50 s | 84 ms | 18x | 1.49 s to 1.52 s | 84 ms to 89 ms | daemon | spliced |
| just 1.43.0 | Add a function | 1.62 s | 63 ms | 26x | 1.62 s to 1.63 s | 62 ms to 64 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 3.00 s | 136 ms | 22x | 2.97 s to 3.27 s | 131 ms to 202 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 43 ms | 17 ms | 2.5x | 43 ms to 43 ms | 17 ms to 17 ms | daemon |  |
| axum 0.8.6 | Edit a function | 803 ms | 51 ms | 16x | 801 ms to 810 ms | 51 ms to 56 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 1.37 s | 43 ms | 32x | 1.36 s to 1.37 s | 43 ms to 52 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 805 ms | 52 ms | 15x | 801 ms to 818 ms | 52 ms to 55 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 73 ms | 18 ms | 4.1x | 73 ms to 85 ms | 18 ms to 19 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 7.20 s | 323 ms | 22x | 7.18 s to 7.24 s | 320 ms to 341 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 11.36 s | 295 ms | 39x | 11.28 s to 11.39 s | 290 ms to 311 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 11.32 s | 371 ms | 31x | 11.26 s to 11.37 s | 354 ms to 653 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 13.50 s | 239 ms | 56x | 13.45 s to 13.68 s | 235 ms to 264 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 7.17 s | 327 ms | 22x | 7.13 s to 7.22 s | 324 ms to 345 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 15.42 s | 388 ms | 40x | 15.38 s to 15.72 s | 360 ms to 416 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 192 ms | 21 ms | 9.1x | 189 ms to 193 ms | 21 ms to 22 ms | daemon |  |
| helix 25.07.1 | Edit a function | 6.84 s | 82 ms | 83x | 6.80 s to 6.84 s | 81 ms to 57.53 s | daemon | full link: no executable to add to |
| helix 25.07.1 | Edit a widely used function | 8.20 s | 95 ms | 86x | 8.16 s to 8.23 s | 94 ms to 96 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 7.59 s | 267 ms | 28x | 7.58 s to 7.64 s | 250 ms to 464 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 6.87 s | 142 ms | 48x | 6.82 s to 6.89 s | 139 ms to 152 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 14.92 s | 350 ms | 43x | 14.82 s to 15.54 s | 347 ms to 365 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 92 ms | 18 ms | 5.1x | 92 ms to 92 ms | 18 ms to 102 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 22.81 s | 884 ms | 26x | 22.68 s to 22.85 s | 883 ms to 1.15 s | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 32.13 s | 352 ms | 91x | 32.03 s to 32.22 s | 349 ms to 442 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 22.85 s | 945 ms | 24x | 22.75 s to 22.96 s | 941 ms to 1.16 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 217 ms | 24 ms | 9.0x | 215 ms to 235 ms | 23 ms to 24 ms | daemon |  |

#### Cold, Cranelift, Linux aarch64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 6.11 s (6.05 s to 6.12 s) | 6.31 s (6.29 s to 6.34 s) | 6.86 s (6.85 s to 6.90 s) | 6.67 s (6.66 s to 6.72 s) | 0 | 2,095 MB | 18 MB |
| fd 10.3.0 | 32.35 s (32.08 s to 32.36 s) | 32.51 s (32.42 s to 32.54 s) | 33.33 s (33.24 s to 33.36 s) | 32.86 s (32.77 s to 32.94 s) | 0 | 1,820 MB | 17 MB |
| bat 0.26.0 | 12.98 s (12.88 s to 13.09 s) | 13.10 s (12.91 s to 13.22 s) | 14.98 s (14.79 s to 15.03 s) | 13.90 s (13.78 s to 14.43 s) | 0 | 2,222 MB | 31 MB |
| hyperfine 1.19.0 | 6.21 s (6.21 s to 6.25 s) | 6.44 s (6.40 s to 6.46 s) | 7.52 s (7.22 s to 7.54 s) | 6.96 s (6.94 s to 7.24 s) | 0 | 2,042 MB | 16 MB |
| just 1.43.0 | 9.09 s (8.98 s to 9.14 s) | 9.01 s (8.94 s to 9.20 s) | 10.36 s (10.29 s to 10.55 s) | 9.69 s (9.65 s to 10.14 s) | 0 | 2,556 MB | 22 MB |
| axum 0.8.6 | 8.91 s (8.87 s to 8.91 s) | 9.21 s (9.17 s to 9.23 s) | 11.09 s (11.05 s to 11.37 s) | 10.13 s (10.04 s to 10.87 s) | 0 | 2,380 MB | 18 MB |
| nushell 0.108.0 | 46.63 s (46.45 s to 46.92 s) | 41.82 s (41.52 s to 42.24 s) | 47.69 s (47.39 s to 48.12 s) | 45.02 s (44.76 s to 46.73 s) | 0 | 11,547 MB | 84 MB |
| helix 25.07.1 | 42.28 s (41.58 s to 42.57 s) | 37.54 s (37.38 s to 37.61 s) | 40.76 s (40.59 s to 41.02 s) | 40.94 s (40.81 s to 40.98 s) | 0 | 10,612 MB | 362 MB |
| bevy 0.20.0 | 119.12 s (117.34 s to 119.93 s) | 101.44 s (100.21 s to 101.73 s) | 126.30 s (124.98 s to 133.39 s) | 124.32 s (123.69 s to 131.26 s) | 0 | 32,608 MB | 311 MB |

#### Memory and disk, Cranelift, Linux aarch64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 1.9 GB | 2.5 GB | 416 MB | 1.7 GB | 1.5 GB |
| ripgrep 15.1.0 | Edit a function | 464 MB | 1.7 GB | 274 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a widely used function | 488 MB | 1.6 GB | 286 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Change a signature | 464 MB | 1.7 GB | 271 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a generic function | 486 MB | 1.6 GB | 281 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Add a function | 463 MB | 1.6 GB | 275 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Add a struct field | 506 MB | 1.6 GB | 331 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Nothing changed | 35 MB | 1.8 GB | 35 MB | 1.7 GB | 1.7 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.2 GB | 3.3 GB | 598 MB | 1.7 GB | 1.7 GB |
| fd 10.3.0 | Edit a function | 520 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Edit a widely used function | 513 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Change a signature | 523 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Add a function | 521 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Add a struct field | 548 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Nothing changed | 57 MB | 1.6 GB | 57 MB | 1.5 GB | 1.6 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.4 GB | 5.8 GB | 460 MB | 2.2 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 672 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Edit a widely used function | 670 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Change a signature | 655 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Add a function | 672 MB | 1.9 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Add a struct field | 688 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Nothing changed | 58 MB | 2.0 GB | 58 MB | 2.0 GB | 2.0 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.2 GB | 2.5 GB | 344 MB | 2.0 GB | 1.9 GB |
| hyperfine 1.19.0 | Edit a function | 361 MB | 1.8 GB | 223 MB | 1.7 GB | 1.7 GB |
| hyperfine 1.19.0 | Edit a widely used function | 371 MB | 1.7 GB | 239 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Change a signature | 372 MB | 1.7 GB | 243 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a function | 362 MB | 1.7 GB | 226 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a struct field | 385 MB | 1.7 GB | 272 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Nothing changed | 79 MB | 1.8 GB | 79 MB | 1.7 GB | 1.8 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.0 GB | 3.6 GB | 606 MB | 2.2 GB | 1.8 GB |
| just 1.43.0 | Edit a function | 434 MB | 2.1 GB | 319 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Edit a widely used function | 422 MB | 2.0 GB | 344 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Change a signature | 422 MB | 2.0 GB | 342 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Add a function | 432 MB | 2.0 GB | 361 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Add a struct field | 640 MB | 2.0 GB | 597 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Nothing changed | 79 MB | 2.1 GB | 79 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.1 GB | 3.1 GB | 507 MB | 2.4 GB | 2.0 GB |
| axum 0.8.6 | Edit a function | 399 MB | 1.9 GB | 230 MB | 1.9 GB | 1.9 GB |
| axum 0.8.6 | Edit a widely used function | 397 MB | 1.9 GB | 318 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Add a function | 400 MB | 1.9 GB | 232 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Nothing changed | 83 MB | 2.0 GB | 83 MB | 2.0 GB | 2.0 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.5 GB | 7.5 GB | 2.0 GB | 5.5 GB | 5.2 GB |
| nushell 0.108.0 | Edit a function | 2.4 GB | 7.6 GB | 2.0 GB | 7.5 GB | 7.5 GB |
| nushell 0.108.0 | Edit a widely used function | 2.3 GB | 7.5 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Change a signature | 2.3 GB | 7.5 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Edit a generic function | 2.4 GB | 7.6 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Add a function | 2.4 GB | 7.6 GB | 2.0 GB | 7.5 GB | 7.5 GB |
| nushell 0.108.0 | Add a struct field | 2.4 GB | 7.6 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Nothing changed | 126 MB | 8.0 GB | 126 MB | 7.9 GB | 8.0 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.5 GB | 5.8 GB | 1.5 GB | 4.1 GB | 3.7 GB |
| helix 25.07.1 | Edit a function | 1.8 GB | 6.6 GB | 1.5 GB | 6.6 GB | 6.6 GB |
| helix 25.07.1 | Edit a widely used function | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Change a signature | 1.7 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a function | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a struct field | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Nothing changed | 148 MB | 10.2 GB | 148 MB | 10.1 GB | 10.2 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 6.8 GB | 9.3 GB | 6.3 GB | 9.2 GB | 8.1 GB |
| bevy 0.20.0 | Edit a function | 6.7 GB | 17.8 GB | 6.3 GB | 17.6 GB | 17.7 GB |
| bevy 0.20.0 | Edit a widely used function | 6.7 GB | 17.8 GB | 6.3 GB | 17.6 GB | 17.7 GB |
| bevy 0.20.0 | Add a function | 6.7 GB | 17.8 GB | 6.3 GB | 17.6 GB | 17.7 GB |
| bevy 0.20.0 | Nothing changed | 240 MB | 32.9 GB | 240 MB | 32.7 GB | 33.0 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.5 GB | 1.7 GB | 1.6 GB after 65 builds | −130 MB | 2.5 GB, 1.6 GB | 404 MB | 502 MB | 16 MB | 0 MB |
| fd 10.3.0 | 1.7 GB | 1.6 GB | 1.5 GB after 55 builds | −139 MB | 3.2 GB, 1.5 GB | 678 MB | 786 MB | 15 MB | 0 MB |
| bat 0.26.0 | 1.9 GB | 2.0 GB | 1.9 GB after 55 builds | −97 MB | 5.8 GB, 1.8 GB | 980 MB | 1.2 GB | 28 MB | 0 MB |
| hyperfine 1.19.0 | 1.9 GB | 1.8 GB | 1.7 GB after 55 builds | −98 MB | 2.5 GB, 1.9 GB | 340 MB | 418 MB | 14 MB | 0 MB |
| just 1.43.0 | 1.8 GB | 2.1 GB | 2.0 GB after 55 builds | −101 MB | 3.6 GB, 1.8 GB | 664 MB | 764 MB | 20 MB | 0 MB |
| axum 0.8.6 | 2.0 GB | 2.0 GB | 1.9 GB after 35 builds | −124 MB | 3.0 GB, 1.9 GB | 405 MB | 439 MB | 15 MB | 0 MB |
| nushell 0.108.0 | 5.2 GB | 8.0 GB | 7.5 GB after 65 builds | −471 MB | 9.5 GB, 5.0 GB | 5.3 GB | 5.5 GB | 81 MB | 1 MB |
| helix 25.07.1 | 3.7 GB | 10.2 GB | 6.6 GB after 55 builds | −3.7 GB | 6.1 GB, 3.7 GB | 3.7 GB | 4.3 GB | 357 MB | 1 MB |
| bevy 0.20.0 | 8.1 GB | 33.0 GB | 17.7 GB after 35 builds | −15.3 GB | 8.7 GB, 7.3 GB | 17.5 GB | 15.7 GB | 241 MB | 1 MB |

#### How fast the program runs, Cranelift, Linux aarch64

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.10 s (1.10 s to 1.10 s); 50 MB, 200.1 MB | 1.08 s (1.08 s to 1.09 s); 50 MB, 51.4 MB | 54 ms (54 ms to 54 ms); 50 MB, 31.6 MB |

#### Edits, LLVM, Linux aarch64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 1.20 s | 72 ms | 17x | 1.20 s to 1.21 s | 71 ms to 88 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 1.56 s | 67 ms | 23x | 1.56 s to 1.57 s | 66 ms to 105 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 1.36 s | 116 ms | 12x | 1.36 s to 1.38 s | 115 ms to 152 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 1.53 s | 66 ms | 23x | 1.53 s to 1.57 s | 66 ms to 67 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 1.21 s | 78 ms | 15x | 1.20 s to 1.21 s | 77 ms to 78 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 1.72 s | 75 ms | 23x | 1.72 s to 1.77 s | 75 ms to 84 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 25 ms | 28 ms | 0.9x | 25 ms to 25 ms | 27 ms to 29 ms | daemon |  |
| fd 10.3.0 | Edit a function | 1.28 s | 69 ms | 19x | 1.28 s to 1.29 s | 66 ms to 142 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 1.24 s | 49 ms | 25x | 1.23 s to 1.26 s | 47 ms to 56 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 1.30 s | 74 ms | 18x | 1.29 s to 1.30 s | 73 ms to 188 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 1.28 s | 72 ms | 18x | 1.27 s to 1.29 s | 71 ms to 106 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 1.55 s | 61 ms | 25x | 1.54 s to 1.56 s | 61 ms to 68 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 34 ms | 27 ms | 1.3x | 34 ms to 34 ms | 27 ms to 27 ms | daemon |  |
| bat 0.26.0 | Edit a function | 1.62 s | 65 ms | 25x | 1.62 s to 1.63 s | 64 ms to 84 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 1.93 s | 66 ms | 29x | 1.92 s to 1.94 s | 65 ms to 67 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 1.87 s | 67 ms | 28x | 1.87 s to 1.90 s | 66 ms to 97 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 1.62 s | 71 ms | 23x | 1.61 s to 1.63 s | 70 ms to 72 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 2.23 s | 71 ms | 31x | 2.21 s to 2.27 s | 70 ms to 73 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 65 ms | 27 ms | 2.4x | 65 ms to 66 ms | 27 ms to 28 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 746 ms | 56 ms | 13x | 743 ms to 751 ms | 56 ms to 58 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 794 ms | 68 ms | 12x | 785 ms to 796 ms | 67 ms to 69 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 796 ms | 104 ms | 7.7x | 793 ms to 810 ms | 103 ms to 112 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 749 ms | 59 ms | 13x | 748 ms to 765 ms | 58 ms to 61 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 877 ms | 87 ms | 10x | 874 ms to 890 ms | 85 ms to 88 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 44 ms | 26 ms | 1.7x | 44 ms to 44 ms | 26 ms to 27 ms | daemon |  |
| just 1.43.0 | Edit a function | 1.50 s | 53 ms | 28x | 1.50 s to 1.51 s | 51 ms to 67 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 1.53 s | 80 ms | 19x | 1.52 s to 1.54 s | 80 ms to 81 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 1.51 s | 103 ms | 15x | 1.50 s to 1.52 s | 102 ms to 107 ms | daemon | spliced |
| just 1.43.0 | Add a function | 1.63 s | 72 ms | 23x | 1.60 s to 1.63 s | 71 ms to 73 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 2.97 s | 140 ms | 21x | 2.96 s to 3.13 s | 137 ms to 202 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 43 ms | 26 ms | 1.7x | 43 ms to 43 ms | 25 ms to 27 ms | daemon |  |
| axum 0.8.6 | Edit a function | 805 ms | 62 ms | 13x | 801 ms to 819 ms | 60 ms to 64 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 1.37 s | 52 ms | 26x | 1.36 s to 1.38 s | 52 ms to 61 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 814 ms | 62 ms | 13x | 805 ms to 817 ms | 61 ms to 66 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 74 ms | 27 ms | 2.7x | 73 ms to 74 ms | 27 ms to 28 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 7.18 s | 332 ms | 22x | 7.17 s to 7.22 s | 330 ms to 508 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 11.30 s | 291 ms | 39x | 11.28 s to 11.41 s | 290 ms to 298 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 11.36 s | 349 ms | 33x | 11.25 s to 11.41 s | 343 ms to 640 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 13.40 s | 457 ms | 29x | 13.39 s to 13.71 s | 447 ms to 490 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 7.18 s | 335 ms | 21x | 7.15 s to 7.19 s | 334 ms to 356 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 15.40 s | 367 ms | 42x | 15.34 s to 15.62 s | 362 ms to 387 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 184 ms | 30 ms | 6.1x | 183 ms to 185 ms | 30 ms to 5.70 s | daemon |  |
| helix 25.07.1 | Edit a function | 6.89 s | 91 ms | 76x | 6.85 s to 6.94 s | 90 ms to 59.30 s | daemon | full link: no executable to add to |
| helix 25.07.1 | Edit a widely used function | 8.26 s | 104 ms | 79x | 8.19 s to 8.27 s | 103 ms to 105 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 7.64 s | 275 ms | 28x | 7.62 s to 7.69 s | 264 ms to 466 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 6.85 s | 145 ms | 47x | 6.83 s to 6.91 s | 142 ms to 154 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 14.98 s | 395 ms | 38x | 14.95 s to 15.58 s | 395 ms to 397 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 93 ms | 27 ms | 3.4x | 92 ms to 93 ms | 27 ms to 91 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 22.79 s | 875 ms | 26x | 22.54 s to 23.64 s | 848 ms to 1.23 s | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 32.11 s | 351 ms | 91x | 31.81 s to 32.47 s | 349 ms to 440 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 22.99 s | 899 ms | 26x | 22.59 s to 23.27 s | 896 ms to 1.12 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 209 ms | 32 ms | 6.5x | 206 ms to 229 ms | 32 ms to 104 ms | daemon |  |

#### Cold, LLVM, Linux aarch64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 6.52 s (6.44 s to 6.63 s) | 6.49 s (6.48 s to 6.51 s) | 7.08 s (7.06 s to 7.09 s) | 6.79 s (6.75 s to 6.82 s) | 0 | 2,194 MB | 17 MB |
| fd 10.3.0 | 33.05 s (33.02 s to 33.20 s) | 32.54 s (32.42 s to 32.73 s) | 33.40 s (33.28 s to 33.59 s) | 32.85 s (32.82 s to 33.25 s) | 0 | 1,991 MB | 17 MB |
| bat 0.26.0 | 13.86 s (13.59 s to 14.04 s) | 13.12 s (13.02 s to 13.20 s) | 15.09 s (14.98 s to 15.16 s) | 14.04 s (14.00 s to 14.53 s) | 0 | 2,410 MB | 30 MB |
| hyperfine 1.19.0 | 6.34 s (6.30 s to 6.39 s) | 6.47 s (6.45 s to 6.49 s) | 7.33 s (7.31 s to 7.35 s) | 6.80 s (6.74 s to 7.30 s) | 0 | 2,149 MB | 15 MB |
| just 1.43.0 | 9.83 s (9.67 s to 10.01 s) | 9.12 s (9.11 s to 9.27 s) | 10.52 s (10.52 s to 10.68 s) | 9.73 s (9.66 s to 10.23 s) | 0 | 2,524 MB | 21 MB |
| axum 0.8.6 | 9.06 s (8.99 s to 9.09 s) | 9.31 s (9.28 s to 9.34 s) | 11.27 s (11.25 s to 11.30 s) | 10.24 s (10.21 s to 11.05 s) | 0 | 2,495 MB | 17 MB |
| nushell 0.108.0 | 51.20 s (50.53 s to 51.30 s) | 42.29 s (42.11 s to 42.50 s) | 48.32 s (47.93 s to 48.39 s) | 44.70 s (44.64 s to 47.08 s) | 0 | 11,293 MB | 78 MB |
| helix 25.07.1 | 44.90 s (44.55 s to 45.22 s) | 38.08 s (37.78 s to 38.19 s) | 41.42 s (41.12 s to 41.53 s) | 41.09 s (41.00 s to 41.28 s) | 0 | 9,515 MB | 352 MB |
| bevy 0.20.0 | 123.18 s (122.06 s to 123.66 s) | 101.55 s (100.65 s to 101.68 s) | 126.12 s (125.16 s to 133.06 s) | 124.08 s (123.64 s to 131.58 s) | 0 | 16,409 MB | 305 MB |

#### Memory and disk, LLVM, Linux aarch64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 2.0 GB | 4.0 GB | 413 MB | 1.8 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a function | 464 MB | 1.8 GB | 273 MB | 1.7 GB | 1.8 GB |
| ripgrep 15.1.0 | Edit a widely used function | 489 MB | 1.8 GB | 285 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Change a signature | 464 MB | 1.8 GB | 271 MB | 1.7 GB | 1.8 GB |
| ripgrep 15.1.0 | Edit a generic function | 485 MB | 1.8 GB | 283 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a function | 465 MB | 1.8 GB | 276 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a struct field | 497 MB | 1.8 GB | 329 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Nothing changed | 50 MB | 1.9 GB | 50 MB | 1.8 GB | 1.9 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.3 GB | 3.4 GB | 592 MB | 1.8 GB | 1.8 GB |
| fd 10.3.0 | Edit a function | 520 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Edit a widely used function | 513 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Change a signature | 523 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Add a function | 521 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Add a struct field | 548 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Nothing changed | 57 MB | 1.8 GB | 57 MB | 1.6 GB | 1.7 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.3 GB | 7.0 GB | 460 MB | 2.2 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 672 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Edit a widely used function | 670 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Change a signature | 655 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Add a function | 672 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Add a struct field | 688 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Nothing changed | 79 MB | 2.2 GB | 79 MB | 2.1 GB | 2.2 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.1 GB | 2.7 GB | 345 MB | 2.1 GB | 2.1 GB |
| hyperfine 1.19.0 | Edit a function | 361 MB | 1.9 GB | 222 MB | 1.8 GB | 1.9 GB |
| hyperfine 1.19.0 | Edit a widely used function | 370 MB | 1.9 GB | 238 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Change a signature | 371 MB | 1.9 GB | 241 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Add a function | 361 MB | 1.9 GB | 224 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Add a struct field | 386 MB | 1.9 GB | 270 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Nothing changed | 79 MB | 1.9 GB | 79 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.1 GB | 4.2 GB | 605 MB | 2.3 GB | 1.9 GB |
| just 1.43.0 | Edit a function | 434 MB | 2.1 GB | 320 MB | 2.0 GB | 2.1 GB |
| just 1.43.0 | Edit a widely used function | 422 MB | 2.1 GB | 344 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Change a signature | 422 MB | 2.1 GB | 342 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Add a function | 432 MB | 2.1 GB | 360 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Add a struct field | 638 MB | 2.1 GB | 594 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Nothing changed | 81 MB | 2.2 GB | 81 MB | 2.1 GB | 2.2 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.1 GB | 3.2 GB | 506 MB | 2.5 GB | 2.2 GB |
| axum 0.8.6 | Edit a function | 399 MB | 2.2 GB | 230 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Edit a widely used function | 397 MB | 2.2 GB | 319 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Add a function | 400 MB | 2.2 GB | 234 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Nothing changed | 83 MB | 2.2 GB | 83 MB | 2.1 GB | 2.2 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.6 GB | 7.8 GB | 2.0 GB | 5.5 GB | 5.2 GB |
| nushell 0.108.0 | Edit a function | 2.4 GB | 7.5 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Edit a widely used function | 2.3 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.4 GB |
| nushell 0.108.0 | Change a signature | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.4 GB |
| nushell 0.108.0 | Edit a generic function | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.4 GB |
| nushell 0.108.0 | Add a function | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.5 GB |
| nushell 0.108.0 | Add a struct field | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.5 GB |
| nushell 0.108.0 | Nothing changed | 126 MB | 8.4 GB | 126 MB | 8.2 GB | 8.3 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.4 GB | 6.0 GB | 1.5 GB | 4.2 GB | 3.8 GB |
| helix 25.07.1 | Edit a function | 1.8 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Edit a widely used function | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Change a signature | 1.7 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a function | 1.8 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a struct field | 1.8 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Nothing changed | 159 MB | 9.6 GB | 159 MB | 9.5 GB | 9.6 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 6.8 GB | 9.5 GB | 6.3 GB | 9.3 GB | 8.2 GB |
| bevy 0.20.0 | Edit a function | 6.7 GB | 17.7 GB | 6.3 GB | 17.5 GB | 17.6 GB |
| bevy 0.20.0 | Edit a widely used function | 6.7 GB | 17.7 GB | 6.3 GB | 17.4 GB | 17.6 GB |
| bevy 0.20.0 | Add a function | 6.7 GB | 17.7 GB | 6.3 GB | 17.5 GB | 17.6 GB |
| bevy 0.20.0 | Nothing changed | 320 MB | 16.2 GB | 320 MB | 16.0 GB | 16.2 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.7 GB | 1.9 GB | 1.7 GB after 65 builds | −148 MB | 3.2 GB, 1.8 GB | 404 MB | 502 MB | 15 MB | 1 MB |
| fd 10.3.0 | 1.8 GB | 1.7 GB | 1.6 GB after 55 builds | −88 MB | 3.4 GB, 1.6 GB | 678 MB | 784 MB | 15 MB | 1 MB |
| bat 0.26.0 | 1.9 GB | 2.2 GB | 2.1 GB after 55 builds | −62 MB | 5.8 GB, 1.8 GB | 980 MB | 1.2 GB | 28 MB | 0 MB |
| hyperfine 1.19.0 | 2.1 GB | 1.9 GB | 1.8 GB after 55 builds | −95 MB | 2.7 GB, 2.1 GB | 340 MB | 417 MB | 13 MB | 0 MB |
| just 1.43.0 | 1.9 GB | 2.2 GB | 2.0 GB after 55 builds | −164 MB | 3.7 GB, 1.8 GB | 664 MB | 760 MB | 19 MB | 0 MB |
| axum 0.8.6 | 2.2 GB | 2.2 GB | 2.1 GB after 35 builds | −116 MB | 3.1 GB, 2.0 GB | 405 MB | 439 MB | 15 MB | 0 MB |
| nushell 0.108.0 | 5.2 GB | 8.3 GB | 7.5 GB after 65 builds | −889 MB | 8.4 GB, 5.0 GB | 5.3 GB | 5.5 GB | 75 MB | 1 MB |
| helix 25.07.1 | 3.8 GB | 9.6 GB | 6.6 GB after 55 builds | −3.1 GB | 6.1 GB, 3.8 GB | 3.7 GB | 4.3 GB | 348 MB | 1 MB |
| bevy 0.20.0 | 8.2 GB | 16.2 GB | 17.6 GB after 35 builds | +1.4 GB | 8.8 GB, 7.5 GB | 17.5 GB | 15.7 GB | 221 MB | 1 MB |

#### How fast the program runs, LLVM, Linux aarch64

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.09 s (1.09 s to 1.09 s); 57 MB, 197.9 MB | 1.08 s (1.08 s to 1.08 s); 57 MB, 51.4 MB | 54 ms (54 ms to 55 ms); 57 MB, 31.6 MB |

### Linux aarch64, bun

blaze [`ac1ee2dd68`](https://github.com/indexable-inc/blaze/commit/ac1ee2dd682b7d321ba75a67924aaf95609572f0), 2026-10-23T08:43:38Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/bun-linux-aarch64.json](benchmarks/bun-linux-aarch64.json).

| Machine | |
|---|---|
| Name | AWS c8g.24xlarge (dev-aws-eph-blaze-arm64bun), pinned to 16 cores with taskset |
| CPU | Neoverse-V2 |
| Cores | 16 |
| Memory (GiB) | 185 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.100.0-nightly (574ff7d98 2026-09-14) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 23.1.3 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 6.9, 5.2, 3.0 |

#### Edits, Cranelift, Linux aarch64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 10.63 s | 486 ms | 22x | 10.59 s to 10.65 s | 483 ms to 497 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 19.64 s | 477 ms | 41x | 19.63 s to 19.66 s | 477 ms to 480 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 10.67 s | 629 ms | 17x | 10.66 s to 10.68 s | 611 ms to 725 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 19.97 s | 503 ms | 40x | 19.96 s to 20.03 s | 501 ms to 508 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.75 s | 735 ms | 20x | 14.74 s to 15.13 s | 731 ms to 738 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 90 ms | 21 ms | 4.3x | 90 ms to 90 ms | 21 ms to 22 ms | daemon |  |

#### Cold, Cranelift, Linux aarch64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 89.03 s | 87.68 s | 91.44 s | 90.72 s | 0 | 36,189 MB | 92 MB |

#### Memory and disk, Cranelift, Linux aarch64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 15.8 GB | 3.2 GB | 12.2 GB | 12.3 GB |
| bun 1.4.3 | Edit a function | 3.5 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Edit a widely used function | 3.6 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Change a signature | 3.5 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Edit a generic function | 3.6 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Add a function | 3.5 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Nothing changed | 60 MB | 27.7 GB | 60 MB | 27.6 GB | 27.7 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 12.3 GB | 27.7 GB | 27.7 GB after 33 builds | +55 MB | 15.9 GB, 12.2 GB | 5.6 GB | 6.6 GB | 88 MB | 1 MB |

#### How fast the program runs, Cranelift, Linux aarch64, bun

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 3.51 s (3.51 s to 3.52 s); 105 MB, 601.0 MB | 2.03 s (2.03 s to 2.03 s); 105 MB, 408.4 MB | 189 ms (188 ms to 189 ms); 105 MB, 318.7 MB |

#### Edits, LLVM, Linux aarch64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 10.66 s | 493 ms | 22x | 10.65 s to 10.67 s | 490 ms to 508 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 19.66 s | 483 ms | 41x | 19.59 s to 19.74 s | 481 ms to 494 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 10.72 s | 609 ms | 18x | 10.67 s to 10.74 s | 608 ms to 714 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 19.97 s | 518 ms | 39x | 19.92 s to 20.04 s | 518 ms to 529 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.80 s | 711 ms | 21x | 14.79 s to 15.09 s | 710 ms to 721 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 90 ms | 32 ms | 2.8x | 90 ms to 90 ms | 31 ms to 43 ms | daemon |  |

#### Cold, LLVM, Linux aarch64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 94.97 s | 89.03 s | 92.67 s | 90.46 s | 0 | 34,829 MB | 60 MB |

#### Memory and disk, LLVM, Linux aarch64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 15.8 GB | 3.2 GB | 12.1 GB | 12.2 GB |
| bun 1.4.3 | Edit a function | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Edit a widely used function | 3.6 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Change a signature | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Edit a generic function | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Add a function | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Nothing changed | 105 MB | 27.2 GB | 105 MB | 27.1 GB | 27.2 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 12.2 GB | 27.2 GB | 27.2 GB after 33 builds | +38 MB | 15.7 GB, 11.6 GB | 5.6 GB | 6.4 GB | 56 MB | 1 MB |

#### How fast the program runs, LLVM, Linux aarch64, bun

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 1.90 s (1.89 s to 1.90 s); 131 MB, 538.2 MB | 2.03 s (2.03 s to 2.06 s); 131 MB, 408.4 MB | 189 ms (188 ms to 190 ms); 131 MB, 318.7 MB |

### Linux x86-64

blaze [`1a005421b7`](https://github.com/indexable-inc/blaze/commit/1a005421b7a47fa3968bc5de6645e25a50dfd855), 2026-10-25T02:46:55Z, Rust 1.99.0, 5 runs of each edit, measured by hand. Raw: [benchmarks/linux-x86-64.json](benchmarks/linux-x86-64.json).

| Machine | |
|---|---|
| Name | AWS c8a.metal-24xl (dev-aws-3), pinned to 16 cores with taskset |
| CPU | AMD EPYC 9R45 96-Core Processor |
| Cores | 16 |
| Memory (GiB) | 188 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 20.1.8 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 7.8, 2.9, 1.1 |

#### Edits, Cranelift, Linux x86-64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 508 ms | 56 ms | 9.1x | 504 ms to 516 ms | 55 ms to 66 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 781 ms | 53 ms | 15x | 778 ms to 784 ms | 52 ms to 88 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 625 ms | 88 ms | 7.1x | 623 ms to 627 ms | 85 ms to 126 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 754 ms | 51 ms | 15x | 750 ms to 757 ms | 51 ms to 52 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 512 ms | 60 ms | 8.5x | 509 ms to 513 ms | 58 ms to 61 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 896 ms | 56 ms | 16x | 891 ms to 923 ms | 55 ms to 85 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 22 ms | 16 ms | 1.4x | 22 ms to 23 ms | 15 ms to 16 ms | daemon |  |
| fd 10.3.0 | Edit a function | 494 ms | 53 ms | 9.3x | 489 ms to 500 ms | 52 ms to 64 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 459 ms | 33 ms | 14x | 457 ms to 467 ms | 33 ms to 36 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 498 ms | 57 ms | 8.7x | 495 ms to 503 ms | 56 ms to 124 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 491 ms | 55 ms | 8.9x | 490 ms to 493 ms | 55 ms to 57 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 683 ms | 45 ms | 15x | 681 ms to 706 ms | 43 ms to 45 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 33 ms | 15 ms | 2.2x | 31 ms to 33 ms | 15 ms to 16 ms | daemon |  |
| bat 0.26.0 | Edit a function | 502 ms | 49 ms | 10x | 498 ms to 506 ms | 48 ms to 63 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 730 ms | 49 ms | 15x | 720 ms to 732 ms | 48 ms to 50 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 689 ms | 51 ms | 14x | 680 ms to 699 ms | 50 ms to 77 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 502 ms | 53 ms | 9.5x | 498 ms to 505 ms | 53 ms to 55 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 958 ms | 56 ms | 17x | 953 ms to 988 ms | 55 ms to 57 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 60 ms | 15 ms | 4.0x | 59 ms to 61 ms | 15 ms to 16 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 385 ms | 35 ms | 11x | 381 ms to 387 ms | 34 ms to 37 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 417 ms | 52 ms | 8.0x | 414 ms to 438 ms | 51 ms to 52 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 423 ms | 76 ms | 5.6x | 419 ms to 426 ms | 74 ms to 80 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 384 ms | 37 ms | 10x | 384 ms to 385 ms | 37 ms to 43 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 485 ms | 62 ms | 7.8x | 480 ms to 490 ms | 62 ms to 84 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 41 ms | 15 ms | 2.7x | 41 ms to 42 ms | 15 ms to 16 ms | daemon |  |
| just 1.43.0 | Edit a function | 707 ms | 41 ms | 17x | 699 ms to 711 ms | 40 ms to 42 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 729 ms | 60 ms | 12x | 722 ms to 733 ms | 60 ms to 61 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 711 ms | 71 ms | 10x | 702 ms to 716 ms | 71 ms to 76 ms | daemon | spliced |
| just 1.43.0 | Add a function | 795 ms | 55 ms | 14x | 783 ms to 801 ms | 54 ms to 56 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 1.83 s | 113 ms | 16x | 1.82 s to 1.93 s | 112 ms to 172 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 41 ms | 15 ms | 2.7x | 40 ms to 41 ms | 14 ms to 15 ms | daemon |  |
| axum 0.8.6 | Edit a function | 427 ms | 42 ms | 10x | 415 ms to 433 ms | 41 ms to 43 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 865 ms | 35 ms | 25x | 865 ms to 870 ms | 35 ms to 49 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 423 ms | 43 ms | 9.8x | 419 ms to 429 ms | 41 ms to 43 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 74 ms | 16 ms | 4.6x | 73 ms to 75 ms | 16 ms to 20 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.25 s | 236 ms | 5.3x | 1.25 s to 1.27 s | 227 ms to 250 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.75 s | 200 ms | 24x | 4.74 s to 4.80 s | 196 ms to 230 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.83 s | 278 ms | 17x | 4.82 s to 4.86 s | 271 ms to 519 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.46 s | 256 ms | 25x | 6.43 s to 6.65 s | 251 ms to 269 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 1.26 s | 240 ms | 5.2x | 1.26 s to 1.27 s | 235 ms to 244 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 8.23 s | 292 ms | 28x | 8.15 s to 8.41 s | 280 ms to 368 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 180 ms | 18 ms | 10x | 177 ms to 183 ms | 18 ms to 19 ms | daemon |  |
| helix 25.07.1 | Edit a function | 2.15 s | 116 ms | 19x | 2.15 s to 2.19 s | 114 ms to 116 ms | daemon | spliced |
| helix 25.07.1 | Edit a widely used function | 3.29 s | 67 ms | 49x | 3.25 s to 3.32 s | 63 ms to 68 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 2.80 s | 217 ms | 13x | 2.78 s to 2.82 s | 216 ms to 587 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 2.16 s | 157 ms | 14x | 2.15 s to 2.18 s | 150 ms to 161 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 8.72 s | 229 ms | 38x | 8.68 s to 9.18 s | 221 ms to 230 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 83 ms | 16 ms | 5.2x | 82 ms to 85 ms | 15 ms to 16 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 3.20 s | 874 ms | 3.7x | 3.02 s to 3.22 s | 817 ms to 879 ms | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 11.60 s | 699 ms | 17x | 11.32 s to 11.69 s | 584 ms to 728 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 3.16 s | 941 ms | 3.4x | 3.12 s to 3.20 s | 902 ms to 1.17 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 183 ms | 19 ms | 9.6x | 179 ms to 197 ms | 19 ms to 550 ms | daemon |  |
| biome @biomejs/biome@2.6.3 | Edit a function | 2.12 s | 2.38 s | 0.9x | 2.04 s to 2.17 s | 2.13 s to 4.78 s | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Edit a widely used function | 9.86 s | 381 ms | 26x | 9.76 s to 10.25 s | 374 ms to 210.60 s | daemon | full link: no executable to add to |
| biome @biomejs/biome@2.6.3 | Change a signature | 8.06 s | 108.75 s | 0.1x | 8.06 s to 8.06 s | 108.75 s to 108.75 s | cargo: `field` is not compiled by blaze and rustc's copy is from before the edi |  |
| biome @biomejs/biome@2.6.3 | Edit a generic function | 9.55 s | 385 ms | 25x | 9.49 s to 9.63 s | 378 ms to 392 ms | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Add a function | 1.95 s | 472 ms | 4.1x | 1.94 s to 1.95 s | 461 ms to 477 ms | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Add a struct field | 4.07 s | 542 ms | 7.5x | 4.03 s to 4.11 s | 540 ms to 577 ms | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Nothing changed | 205 ms | 32 ms | 6.4x | 203 ms to 320 ms | 31 ms to 13.02 s | daemon |  |

#### Cold, Cranelift, Linux x86-64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 4.67 s (4.63 s to 4.71 s) | 5.20 s (5.19 s to 5.23 s) | 5.75 s (5.74 s to 5.78 s) | 5.38 s (5.33 s to 5.45 s) | 0 | 2,034 MB | 93 MB |
| fd 10.3.0 | 26.10 s (25.95 s to 26.14 s) | 29.57 s (29.45 s to 29.63 s) | 30.13 s (30.00 s to 30.18 s) | 29.77 s (29.60 s to 30.25 s) | 0 | 1,735 MB | 1,470 MB |
| bat 0.26.0 | 9.95 s (9.80 s to 10.04 s) | 13.44 s (13.38 s to 13.52 s) | 14.56 s (14.51 s to 14.87 s) | 13.20 s (12.74 s to 13.32 s) | 0 | 2,150 MB | 243 MB |
| hyperfine 1.19.0 | 4.70 s (4.68 s to 4.71 s) | 7.64 s (7.53 s to 7.65 s) | 8.99 s (8.87 s to 9.20 s) | 7.62 s (7.58 s to 7.89 s) | 0 | 1,774 MB | 92 MB |
| just 1.43.0 | 6.69 s (6.63 s to 6.85 s) | 9.24 s (9.19 s to 9.31 s) | 10.32 s (10.27 s to 10.38 s) | 9.25 s (9.06 s to 9.44 s) | 0 | 2,287 MB | 99 MB |
| axum 0.8.6 | 6.99 s (6.95 s to 7.04 s) | 8.60 s (8.54 s to 8.65 s) | 9.94 s (9.88 s to 9.99 s) | 8.91 s (8.79 s to 9.44 s) | 0 | 2,315 MB | 93 MB |
| nushell 0.108.0 | 34.74 s (34.59 s to 35.46 s) | 36.44 s (35.94 s to 36.87 s) | 40.42 s (39.92 s to 40.86 s) | 37.15 s (36.78 s to 38.56 s) | 0 | 11,040 MB | 295 MB |
| helix 25.07.1 | 30.11 s (29.91 s to 30.32 s) | 31.17 s (31.13 s to 31.48 s) | 34.62 s (34.31 s to 34.91 s) | 34.11 s (33.92 s to 34.15 s) | 0 | 10,648 MB | 169 MB |
| bevy 0.20.0 | 83.95 s (82.52 s to 84.68 s) | 71.12 s (70.35 s to 71.99 s) | 90.24 s (88.95 s to 93.76 s) | 96.46 s (96.09 s to 104.93 s) | 0 | 27,938 MB | 1,551 MB |
| biome @biomejs/biome@2.6.3 | 90.11 s (89.08 s to 90.89 s) | 81.32 s (80.78 s to 81.63 s) | 98.47 s (98.08 s to 99.98 s) | 240.50 s (237.38 s to 240.87 s) | 0 | 21,105 MB | 26,281 MB |

#### Memory and disk, Cranelift, Linux x86-64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 2.4 GB | 5.2 GB | 421 MB | 1.7 GB | 1.6 GB |
| ripgrep 15.1.0 | Edit a function | 528 MB | 1.7 GB | 294 MB | 1.5 GB | 1.6 GB |
| ripgrep 15.1.0 | Edit a widely used function | 533 MB | 1.7 GB | 306 MB | 1.5 GB | 1.6 GB |
| ripgrep 15.1.0 | Change a signature | 527 MB | 1.7 GB | 291 MB | 1.5 GB | 1.6 GB |
| ripgrep 15.1.0 | Edit a generic function | 537 MB | 1.7 GB | 299 MB | 1.5 GB | 1.6 GB |
| ripgrep 15.1.0 | Add a function | 535 MB | 1.7 GB | 298 MB | 1.5 GB | 1.6 GB |
| ripgrep 15.1.0 | Add a struct field | 550 MB | 1.7 GB | 350 MB | 1.5 GB | 1.6 GB |
| ripgrep 15.1.0 | Nothing changed | 29 MB | 1.8 GB | 29 MB | 1.7 GB | 1.8 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.6 GB | 6.1 GB | 555 MB | 1.8 GB | 1.6 GB |
| fd 10.3.0 | Edit a function | 605 MB | 1.5 GB | 371 MB | 1.3 GB | 1.4 GB |
| fd 10.3.0 | Edit a widely used function | 597 MB | 1.5 GB | 372 MB | 1.3 GB | 1.4 GB |
| fd 10.3.0 | Change a signature | 609 MB | 1.5 GB | 370 MB | 1.3 GB | 1.4 GB |
| fd 10.3.0 | Add a function | 602 MB | 1.5 GB | 369 MB | 1.3 GB | 1.4 GB |
| fd 10.3.0 | Add a struct field | 633 MB | 1.5 GB | 371 MB | 1.3 GB | 1.4 GB |
| fd 10.3.0 | Nothing changed | 111 MB | 1.5 GB | 111 MB | 1.4 GB | 1.5 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.7 GB | 7.9 GB | 534 MB | 2.1 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 747 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Edit a widely used function | 748 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Change a signature | 740 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Add a function | 749 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Add a struct field | 766 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Nothing changed | 534 MB | 2.1 GB | 534 MB | 2.0 GB | 2.1 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.6 GB | 9.6 GB | 534 MB | 1.9 GB | 1.8 GB |
| hyperfine 1.19.0 | Edit a function | 534 MB | 1.6 GB | 534 MB | 1.5 GB | 1.6 GB |
| hyperfine 1.19.0 | Edit a widely used function | 534 MB | 1.6 GB | 534 MB | 1.4 GB | 1.5 GB |
| hyperfine 1.19.0 | Change a signature | 534 MB | 1.6 GB | 534 MB | 1.4 GB | 1.5 GB |
| hyperfine 1.19.0 | Add a function | 534 MB | 1.6 GB | 534 MB | 1.4 GB | 1.5 GB |
| hyperfine 1.19.0 | Add a struct field | 534 MB | 1.6 GB | 534 MB | 1.4 GB | 1.5 GB |
| hyperfine 1.19.0 | Nothing changed | 534 MB | 1.7 GB | 534 MB | 1.5 GB | 1.6 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.5 GB | 8.6 GB | 635 MB | 2.0 GB | 1.8 GB |
| just 1.43.0 | Edit a function | 535 MB | 1.9 GB | 534 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Edit a widely used function | 534 MB | 1.9 GB | 534 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Change a signature | 534 MB | 1.9 GB | 534 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Add a function | 541 MB | 1.9 GB | 534 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Add a struct field | 655 MB | 1.9 GB | 620 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Nothing changed | 534 MB | 2.1 GB | 534 MB | 1.9 GB | 2.0 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.5 GB | 5.8 GB | 534 MB | 2.2 GB | 2.0 GB |
| axum 0.8.6 | Edit a function | 534 MB | 1.9 GB | 534 MB | 1.7 GB | 1.8 GB |
| axum 0.8.6 | Edit a widely used function | 534 MB | 1.8 GB | 534 MB | 1.7 GB | 1.8 GB |
| axum 0.8.6 | Add a function | 534 MB | 1.8 GB | 534 MB | 1.7 GB | 1.8 GB |
| axum 0.8.6 | Nothing changed | 534 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.8 GB | 9.1 GB | 2.4 GB | 5.4 GB | 5.2 GB |
| nushell 0.108.0 | Edit a function | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.2 GB |
| nushell 0.108.0 | Edit a widely used function | 2.7 GB | 7.1 GB | 2.4 GB | 6.9 GB | 7.1 GB |
| nushell 0.108.0 | Change a signature | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.1 GB |
| nushell 0.108.0 | Edit a generic function | 2.7 GB | 7.1 GB | 2.4 GB | 7.0 GB | 7.1 GB |
| nushell 0.108.0 | Add a function | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.1 GB |
| nushell 0.108.0 | Add a struct field | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.1 GB |
| nushell 0.108.0 | Nothing changed | 534 MB | 7.7 GB | 534 MB | 7.5 GB | 7.6 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.8 GB | 8.4 GB | 1.8 GB | 4.1 GB | 4.2 GB |
| helix 25.07.1 | Edit a function | 2.1 GB | 5.8 GB | 1.8 GB | 5.6 GB | 5.7 GB |
| helix 25.07.1 | Edit a widely used function | 2.1 GB | 5.7 GB | 1.8 GB | 5.6 GB | 5.7 GB |
| helix 25.07.1 | Change a signature | 2.1 GB | 5.8 GB | 1.8 GB | 5.6 GB | 5.7 GB |
| helix 25.07.1 | Add a function | 2.1 GB | 5.7 GB | 1.8 GB | 5.6 GB | 5.7 GB |
| helix 25.07.1 | Add a struct field | 2.1 GB | 5.8 GB | 1.8 GB | 5.6 GB | 5.7 GB |
| helix 25.07.1 | Nothing changed | 534 MB | 6.2 GB | 534 MB | 6.1 GB | 6.2 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 8.5 GB | 9.7 GB | 8.1 GB | 9.4 GB | 8.2 GB |
| bevy 0.20.0 | Edit a function | 8.5 GB | 28.9 GB | 8.1 GB | 28.6 GB | 15.0 GB |
| bevy 0.20.0 | Edit a widely used function | 8.5 GB | 20.4 GB | 8.1 GB | 20.1 GB | 20.3 GB |
| bevy 0.20.0 | Add a function | 8.5 GB | 15.2 GB | 8.1 GB | 15.0 GB | 15.2 GB |
| bevy 0.20.0 | Nothing changed | 534 MB | 28.3 GB | 534 MB | 28.0 GB | 28.7 GB |
| biome @biomejs/biome@2.6.3 | Cold, until the daemon is ready | 8.6 GB | 9.6 GB | 2.6 GB | 7.1 GB | 6.9 GB |
| biome @biomejs/biome@2.6.3 | Edit a function | 3.0 GB | 7.6 GB | 2.6 GB | 7.4 GB | 8.6 GB |
| biome @biomejs/biome@2.6.3 | Edit a widely used function | 3.0 GB | 20.9 GB | 2.6 GB | 20.7 GB | 19.3 GB |
| biome @biomejs/biome@2.6.3 | Change a signature | 2.9 GB | 24.3 GB | 2.6 GB | 24.1 GB | 6.2 GB |
| biome @biomejs/biome@2.6.3 | Edit a generic function | 3.0 GB | 19.4 GB | 2.6 GB | 19.1 GB | 19.3 GB |
| biome @biomejs/biome@2.6.3 | Add a function | 3.0 GB | 19.3 GB | 2.6 GB | 19.1 GB | 19.3 GB |
| biome @biomejs/biome@2.6.3 | Add a struct field | 3.0 GB | 19.3 GB | 2.6 GB | 19.1 GB | 19.3 GB |
| biome @biomejs/biome@2.6.3 | Nothing changed | 534 MB | 6.3 GB | 534 MB | 6.1 GB | 6.3 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.6 GB | 1.8 GB | 1.6 GB after 65 builds | −137 MB | 4.9 GB, 1.7 GB | 407 MB | 760 MB | 91 MB | 1 MB |
| fd 10.3.0 | 1.6 GB | 1.5 GB | 1.4 GB after 55 builds | −93 MB | 6.5 GB, 1.4 GB | 571 MB | 1.0 GB | 321 MB | 0 MB |
| bat 0.26.0 | 1.9 GB | 2.1 GB | 2.0 GB after 55 builds | −133 MB | 7.5 GB, 1.8 GB | 973 MB | 1.9 GB | 130 MB | 0 MB |
| hyperfine 1.19.0 | 1.8 GB | 1.6 GB | 1.5 GB after 55 builds | −96 MB | 8.2 GB, 1.8 GB | 342 MB | 1.1 GB | 91 MB | 0 MB |
| just 1.43.0 | 1.8 GB | 2.0 GB | 1.9 GB after 55 builds | −144 MB | 7.8 GB, 1.8 GB | 667 MB | 1.4 GB | 97 MB | 0 MB |
| axum 0.8.6 | 2.0 GB | 2.0 GB | 1.8 GB after 35 builds | −140 MB | 5.2 GB, 2.0 GB | 404 MB | 733 MB | 91 MB | 1 MB |
| nushell 0.108.0 | 5.2 GB | 7.6 GB | 7.1 GB after 65 builds | −540 MB | 8.7 GB, 5.0 GB | 5.3 GB | 7.6 GB | 187 MB | 1 MB |
| helix 25.07.1 | 4.2 GB | 6.2 GB | 5.7 GB after 55 builds | −502 MB | 7.7 GB, 3.8 GB | 3.7 GB | 5.6 GB | 155 MB | 1 MB |
| bevy 0.20.0 | 8.2 GB | 28.7 GB | 15.2 GB after 35 builds | −13.5 GB | 11.1 GB, 9.9 GB | 17.5 GB | 17.7 GB | 1.5 GB | 1 MB |
| biome @biomejs/biome@2.6.3 | 6.9 GB | 6.3 GB | 19.3 GB after 57 builds | +13.0 GB | 15.0 GB, 6.4 GB | 11.0 GB | 17.5 GB | 25.1 GB | 2 MB |

#### How fast the program runs, Cranelift, Linux x86-64

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.09 s (1.09 s to 1.09 s); 111 MB, 200.8 MB | 1.07 s (1.07 s to 1.07 s); 111 MB, 53.2 MB | 30 ms (29 ms to 30 ms); 111 MB, 30.7 MB |

#### Edits, LLVM, Linux x86-64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 502 ms | 64 ms | 7.8x | 500 ms to 508 ms | 63 ms to 74 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 781 ms | 62 ms | 13x | 779 ms to 783 ms | 61 ms to 96 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 622 ms | 111 ms | 5.6x | 620 ms to 626 ms | 110 ms to 138 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 753 ms | 66 ms | 11x | 750 ms to 757 ms | 66 ms to 67 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 507 ms | 69 ms | 7.3x | 505 ms to 515 ms | 69 ms to 71 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 894 ms | 67 ms | 13x | 881 ms to 918 ms | 63 ms to 73 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 23 ms | 24 ms | 1.0x | 22 ms to 24 ms | 24 ms to 26 ms | daemon |  |
| fd 10.3.0 | Edit a function | 493 ms | 62 ms | 8.0x | 485 ms to 514 ms | 60 ms to 74 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 461 ms | 44 ms | 10x | 455 ms to 462 ms | 44 ms to 45 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 498 ms | 65 ms | 7.7x | 496 ms to 506 ms | 64 ms to 127 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 493 ms | 65 ms | 7.6x | 491 ms to 497 ms | 63 ms to 68 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 681 ms | 52 ms | 13x | 676 ms to 704 ms | 52 ms to 54 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 33 ms | 23 ms | 1.4x | 32 ms to 33 ms | 23 ms to 25 ms | daemon |  |
| bat 0.26.0 | Edit a function | 498 ms | 57 ms | 8.7x | 490 ms to 501 ms | 56 ms to 75 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 727 ms | 57 ms | 13x | 718 ms to 732 ms | 57 ms to 58 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 691 ms | 59 ms | 12x | 689 ms to 695 ms | 58 ms to 84 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 502 ms | 62 ms | 8.1x | 500 ms to 508 ms | 61 ms to 65 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 958 ms | 63 ms | 15x | 951 ms to 1.02 s | 62 ms to 65 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 60 ms | 24 ms | 2.5x | 58 ms to 62 ms | 23 ms to 25 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 383 ms | 47 ms | 8.1x | 381 ms to 385 ms | 46 ms to 47 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 417 ms | 56 ms | 7.4x | 411 ms to 419 ms | 55 ms to 57 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 421 ms | 84 ms | 5.0x | 417 ms to 424 ms | 80 ms to 87 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 384 ms | 50 ms | 7.7x | 381 ms to 386 ms | 48 ms to 50 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 484 ms | 66 ms | 7.3x | 480 ms to 486 ms | 65 ms to 92 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 42 ms | 24 ms | 1.8x | 41 ms to 43 ms | 23 ms to 26 ms | daemon |  |
| just 1.43.0 | Edit a function | 703 ms | 51 ms | 14x | 700 ms to 710 ms | 47 ms to 56 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 726 ms | 70 ms | 10x | 719 ms to 729 ms | 68 ms to 71 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 709 ms | 89 ms | 8.0x | 703 ms to 717 ms | 88 ms to 90 ms | daemon | spliced |
| just 1.43.0 | Add a function | 792 ms | 65 ms | 12x | 791 ms to 799 ms | 64 ms to 66 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 1.85 s | 122 ms | 15x | 1.83 s to 1.94 s | 120 ms to 175 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 41 ms | 23 ms | 1.8x | 41 ms to 42 ms | 23 ms to 23 ms | daemon |  |
| axum 0.8.6 | Edit a function | 421 ms | 51 ms | 8.3x | 420 ms to 423 ms | 50 ms to 54 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 861 ms | 45 ms | 19x | 857 ms to 873 ms | 44 ms to 57 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 422 ms | 51 ms | 8.3x | 420 ms to 432 ms | 49 ms to 53 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 74 ms | 25 ms | 3.0x | 73 ms to 77 ms | 24 ms to 27 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.25 s | 240 ms | 5.2x | 1.24 s to 1.31 s | 237 ms to 426 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.80 s | 203 ms | 24x | 4.76 s to 4.84 s | 199 ms to 241 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.82 s | 284 ms | 17x | 4.77 s to 4.83 s | 261 ms to 537 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.46 s | 455 ms | 14x | 6.41 s to 6.56 s | 445 ms to 488 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 1.25 s | 248 ms | 5.0x | 1.25 s to 1.27 s | 239 ms to 267 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 8.19 s | 287 ms | 29x | 8.14 s to 8.54 s | 279 ms to 373 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 179 ms | 28 ms | 6.4x | 175 ms to 180 ms | 26 ms to 1.07 s | daemon |  |
| helix 25.07.1 | Edit a function | 2.16 s | 113 ms | 19x | 2.14 s to 2.18 s | 112 ms to 135 ms | daemon | spliced |
| helix 25.07.1 | Edit a widely used function | 3.29 s | 74 ms | 45x | 3.27 s to 3.31 s | 73 ms to 76 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 2.80 s | 228 ms | 12x | 2.78 s to 2.81 s | 225 ms to 604 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 2.15 s | 165 ms | 13x | 2.15 s to 2.17 s | 160 ms to 169 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 8.68 s | 230 ms | 38x | 8.66 s to 9.18 s | 227 ms to 234 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 81 ms | 26 ms | 3.1x | 81 ms to 83 ms | 25 ms to 22.54 s | daemon |  |
| bevy 0.20.0 | Edit a function | 3.14 s | 866 ms | 3.6x | 3.09 s to 3.25 s | 769 ms to 880 ms | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 11.34 s | 717 ms | 16x | 11.19 s to 13.66 s | 611 ms to 775 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 3.15 s | 934 ms | 3.4x | 3.10 s to 3.22 s | 907 ms to 1.12 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 197 ms | 27 ms | 7.3x | 195 ms to 226 ms | 26 ms to 355 ms | daemon |  |
| biome @biomejs/biome@2.6.3 | Edit a function | 2.10 s | 2.08 s | 1.0x | 2.01 s to 2.14 s | 2.03 s to 5.47 s | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Edit a widely used function | 9.89 s | 382 ms | 26x | 9.72 s to 9.90 s | 380 ms to 325.03 s | daemon | full link: no executable to add to |
| biome @biomejs/biome@2.6.3 | Change a signature | 8.06 s | 119.02 s | 0.1x | 8.06 s to 8.06 s | 119.02 s to 119.02 s | cargo: `field` is not compiled by blaze and rustc's copy is from before the edi |  |
| biome @biomejs/biome@2.6.3 | Edit a generic function | 9.60 s | 400 ms | 24x | 9.57 s to 9.99 s | 395 ms to 887 ms | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Add a function | 1.94 s | 471 ms | 4.1x | 1.93 s to 1.97 s | 466 ms to 485 ms | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Add a struct field | 4.06 s | 541 ms | 7.5x | 4.04 s to 4.15 s | 536 ms to 577 ms | daemon | spliced |
| biome @biomejs/biome@2.6.3 | Nothing changed | 200 ms | 40 ms | 5.0x | 199 ms to 244 ms | 40 ms to 14.85 s | daemon |  |

#### Cold, LLVM, Linux x86-64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 5.17 s (5.13 s to 5.26 s) | 5.65 s (5.62 s to 5.75 s) | 6.24 s (6.21 s to 6.33 s) | 5.84 s (5.80 s to 5.89 s) | 0 | 2,041 MB | 63 MB |
| fd 10.3.0 | 26.88 s (26.84 s to 27.00 s) | 31.80 s (31.79 s to 31.84 s) | 32.39 s (32.36 s to 32.43 s) | 32.69 s (32.41 s to 32.96 s) | 0 | 1,773 MB | 1,440 MB |
| bat 0.26.0 | 11.00 s (10.83 s to 11.04 s) | 19.60 s (19.40 s to 19.73 s) | 20.72 s (20.52 s to 20.86 s) | 19.86 s (19.65 s to 20.07 s) | 0 | 2,205 MB | 212 MB |
| hyperfine 1.19.0 | 4.93 s (4.86 s to 5.03 s) | 11.36 s (11.02 s to 11.69 s) | 12.76 s (12.42 s to 13.09 s) | 21.16 s (20.82 s to 21.34 s) | 0 | 1,933 MB | 62 MB |
| just 1.43.0 | 8.04 s (7.88 s to 8.11 s) | 11.22 s (10.98 s to 11.35 s) | 12.35 s (12.11 s to 12.48 s) | 11.36 s (11.25 s to 11.65 s) | 0 | 2,295 MB | 68 MB |
| axum 0.8.6 | 7.13 s (7.11 s to 7.14 s) | 9.59 s (9.48 s to 9.62 s) | 10.98 s (10.88 s to 11.02 s) | 10.07 s (9.99 s to 10.60 s) | 0 | 2,264 MB | 64 MB |
| nushell 0.108.0 | 42.05 s (42.01 s to 42.70 s) | 47.78 s (47.40 s to 49.00 s) | 51.64 s (51.27 s to 52.88 s) | 59.40 s (58.97 s to 60.42 s) | 0 | 10,656 MB | 255 MB |
| helix 25.07.1 | 35.17 s (35.01 s to 35.22 s) | 35.30 s (35.00 s to 35.52 s) | 38.61 s (38.31 s to 38.81 s) | 55.86 s (40.43 s to 56.09 s) | 0 | 9,789 MB | 566 MB |
| bevy 0.20.0 | 97.82 s (96.02 s to 98.94 s) | 77.24 s (76.83 s to 77.61 s) | 95.73 s (95.30 s to 99.05 s) | 103.81 s (102.98 s to 104.75 s) | 0 | 13,336 MB | 1,443 MB |
| biome @biomejs/biome@2.6.3 | 106.07 s (104.46 s to 107.33 s) | 87.76 s (86.77 s to 88.22 s) | 104.97 s (104.21 s to 106.04 s) | 292.04 s (291.12 s to 313.65 s) | 0 | 16,038 MB | 33,824 MB |

#### Memory and disk, LLVM, Linux x86-64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 2.4 GB | 5.7 GB | 448 MB | 1.7 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a function | 529 MB | 1.8 GB | 294 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a widely used function | 538 MB | 1.8 GB | 304 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Change a signature | 530 MB | 1.8 GB | 294 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a generic function | 537 MB | 1.8 GB | 300 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a function | 534 MB | 1.8 GB | 297 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a struct field | 545 MB | 1.8 GB | 350 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Nothing changed | 111 MB | 1.8 GB | 111 MB | 1.6 GB | 1.8 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.6 GB | 7.0 GB | 554 MB | 1.8 GB | 1.6 GB |
| fd 10.3.0 | Edit a function | 605 MB | 1.7 GB | 534 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Edit a widely used function | 595 MB | 1.6 GB | 534 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Change a signature | 610 MB | 1.6 GB | 534 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Add a function | 604 MB | 1.6 GB | 534 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Add a struct field | 634 MB | 1.6 GB | 534 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Nothing changed | 534 MB | 1.7 GB | 534 MB | 1.5 GB | 1.7 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.9 GB | 9.5 GB | 534 MB | 2.1 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 749 MB | 2.1 GB | 534 MB | 1.9 GB | 2.0 GB |
| bat 0.26.0 | Edit a widely used function | 748 MB | 2.1 GB | 534 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Change a signature | 741 MB | 2.1 GB | 534 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Add a function | 749 MB | 2.1 GB | 534 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Add a struct field | 766 MB | 2.1 GB | 534 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Nothing changed | 534 MB | 2.2 GB | 534 MB | 2.0 GB | 2.2 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.6 GB | 11.6 GB | 534 MB | 1.9 GB | 1.8 GB |
| hyperfine 1.19.0 | Edit a function | 534 MB | 1.8 GB | 534 MB | 1.6 GB | 1.8 GB |
| hyperfine 1.19.0 | Edit a widely used function | 534 MB | 1.8 GB | 534 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Change a signature | 534 MB | 1.8 GB | 534 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a function | 534 MB | 1.8 GB | 534 MB | 1.6 GB | 1.8 GB |
| hyperfine 1.19.0 | Add a struct field | 534 MB | 1.8 GB | 534 MB | 1.6 GB | 1.8 GB |
| hyperfine 1.19.0 | Nothing changed | 534 MB | 1.9 GB | 534 MB | 1.7 GB | 1.8 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.4 GB | 10.9 GB | 631 MB | 2.0 GB | 1.8 GB |
| just 1.43.0 | Edit a function | 539 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Edit a widely used function | 534 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Change a signature | 534 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Add a function | 538 MB | 2.0 GB | 534 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Add a struct field | 654 MB | 2.0 GB | 620 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Nothing changed | 534 MB | 2.2 GB | 534 MB | 1.9 GB | 2.1 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.5 GB | 6.1 GB | 534 MB | 2.3 GB | 2.2 GB |
| axum 0.8.6 | Edit a function | 534 MB | 2.0 GB | 534 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Edit a widely used function | 534 MB | 2.0 GB | 534 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Add a function | 534 MB | 2.0 GB | 534 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Nothing changed | 534 MB | 2.1 GB | 534 MB | 1.9 GB | 2.1 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.8 GB | 12.3 GB | 2.4 GB | 5.5 GB | 5.3 GB |
| nushell 0.108.0 | Edit a function | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.1 GB |
| nushell 0.108.0 | Edit a widely used function | 2.7 GB | 7.1 GB | 2.4 GB | 6.8 GB | 7.0 GB |
| nushell 0.108.0 | Change a signature | 2.7 GB | 7.1 GB | 2.4 GB | 6.9 GB | 7.1 GB |
| nushell 0.108.0 | Edit a generic function | 2.7 GB | 7.2 GB | 2.4 GB | 6.9 GB | 7.1 GB |
| nushell 0.108.0 | Add a function | 2.7 GB | 7.2 GB | 2.4 GB | 6.9 GB | 7.1 GB |
| nushell 0.108.0 | Add a struct field | 2.7 GB | 7.2 GB | 2.4 GB | 6.9 GB | 7.1 GB |
| nushell 0.108.0 | Nothing changed | 534 MB | 8.0 GB | 534 MB | 7.8 GB | 8.0 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.9 GB | 9.9 GB | 1.8 GB | 4.0 GB | 3.8 GB |
| helix 25.07.1 | Edit a function | 2.1 GB | 6.2 GB | 1.8 GB | 5.9 GB | 5.9 GB |
| helix 25.07.1 | Edit a widely used function | 2.1 GB | 6.0 GB | 1.8 GB | 5.7 GB | 5.9 GB |
| helix 25.07.1 | Change a signature | 2.1 GB | 6.0 GB | 1.8 GB | 5.7 GB | 5.9 GB |
| helix 25.07.1 | Add a function | 2.1 GB | 6.0 GB | 1.8 GB | 5.7 GB | 5.9 GB |
| helix 25.07.1 | Add a struct field | 2.1 GB | 6.0 GB | 1.8 GB | 5.7 GB | 5.9 GB |
| helix 25.07.1 | Nothing changed | 534 MB | 6.7 GB | 534 MB | 6.5 GB | 6.7 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 8.5 GB | 9.7 GB | 8.1 GB | 9.5 GB | 8.5 GB |
| bevy 0.20.0 | Edit a function | 8.5 GB | 12.9 GB | 8.1 GB | 12.6 GB | 12.9 GB |
| bevy 0.20.0 | Edit a widely used function | 8.5 GB | 19.6 GB | 8.1 GB | 19.3 GB | 19.7 GB |
| bevy 0.20.0 | Add a function | 8.5 GB | 14.9 GB | 8.1 GB | 14.6 GB | 14.8 GB |
| bevy 0.20.0 | Nothing changed | 534 MB | 13.3 GB | 534 MB | 12.9 GB | 13.2 GB |
| biome @biomejs/biome@2.6.3 | Cold, until the daemon is ready | 8.1 GB | 11.1 GB | 2.6 GB | 7.1 GB | 6.9 GB |
| biome @biomejs/biome@2.6.3 | Edit a function | 3.0 GB | 7.8 GB | 2.6 GB | 7.5 GB | 9.4 GB |
| biome @biomejs/biome@2.6.3 | Edit a widely used function | 3.0 GB | 21.1 GB | 2.6 GB | 20.8 GB | 20.7 GB |
| biome @biomejs/biome@2.6.3 | Change a signature | 3.0 GB | 24.0 GB | 2.6 GB | 23.2 GB | 6.3 GB |
| biome @biomejs/biome@2.6.3 | Edit a generic function | 3.0 GB | 19.7 GB | 2.6 GB | 19.4 GB | 19.6 GB |
| biome @biomejs/biome@2.6.3 | Add a function | 3.0 GB | 19.7 GB | 2.6 GB | 19.4 GB | 19.6 GB |
| biome @biomejs/biome@2.6.3 | Add a struct field | 3.0 GB | 19.7 GB | 2.6 GB | 19.4 GB | 19.6 GB |
| biome @biomejs/biome@2.6.3 | Nothing changed | 809 MB | 6.4 GB | 809 MB | 6.1 GB | 6.3 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.7 GB | 1.8 GB | 1.7 GB after 65 builds | −86 MB | 5.6 GB, 1.7 GB | 407 MB | 608 MB | 61 MB | 0 MB |
| fd 10.3.0 | 1.6 GB | 1.7 GB | 1.6 GB after 55 builds | −124 MB | 7.7 GB, 1.7 GB | 571 MB | 804 MB | 290 MB | 1 MB |
| bat 0.26.0 | 1.9 GB | 2.2 GB | 2.1 GB after 55 builds | −92 MB | 9.3 GB, 1.9 GB | 973 MB | 1.3 GB | 100 MB | 1 MB |
| hyperfine 1.19.0 | 1.8 GB | 1.8 GB | 1.8 GB after 55 builds | −90 MB | 11.6 GB, 1.8 GB | 342 MB | 604 MB | 59 MB | 0 MB |
| just 1.43.0 | 1.8 GB | 2.1 GB | 2.0 GB after 55 builds | −145 MB | 10.8 GB, 1.9 GB | 667 MB | 956 MB | 66 MB | 0 MB |
| axum 0.8.6 | 2.2 GB | 2.1 GB | 1.9 GB after 35 builds | −115 MB | 6.2 GB, 2.0 GB | 404 MB | 524 MB | 61 MB | 1 MB |
| nushell 0.108.0 | 5.3 GB | 8.0 GB | 7.1 GB after 65 builds | −863 MB | 12.0 GB, 5.1 GB | 5.3 GB | 6.3 GB | 147 MB | 1 MB |
| helix 25.07.1 | 3.8 GB | 6.7 GB | 5.9 GB after 55 builds | −850 MB | 10.1 GB, 3.7 GB | 3.7 GB | 4.8 GB | 552 MB | 1 MB |
| bevy 0.20.0 | 8.5 GB | 13.2 GB | 14.8 GB after 35 builds | +1.6 GB | 11.0 GB, 9.9 GB | 17.5 GB | 16.3 GB | 1.3 GB | 1 MB |
| biome @biomejs/biome@2.6.3 | 6.9 GB | 6.3 GB | 19.6 GB after 57 builds | +13.3 GB | 17.6 GB, 6.4 GB | 11.0 GB | 15.9 GB | 34.3 GB | 2 MB |

#### How fast the program runs, LLVM, Linux x86-64

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.08 s (1.07 s to 1.08 s); 111 MB, 199.8 MB | 1.08 s (1.07 s to 1.08 s); 111 MB, 53.2 MB | 30 ms (28 ms to 31 ms); 111 MB, 30.7 MB |

### Linux x86-64, bun

blaze [`961bb3105a`](https://github.com/indexable-inc/blaze/commit/961bb3105a8e90e0ac33488c803f5513f075bb87), 2026-10-23T15:45:27Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/bun-linux-x86-64.json](benchmarks/bun-linux-x86-64.json).

| Machine | |
|---|---|
| Name | AWS c8a.24xlarge (dev-aws-eph-blaze-x86edits), pinned to 16 cores with taskset |
| CPU | AMD EPYC 9R45 |
| Cores | 16 |
| Memory (GiB) | 185 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.100.0-nightly (574ff7d98 2026-09-14) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 23.1.3 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 12.4, 6.4, 2.7 |

#### Edits, Cranelift, Linux x86-64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 9.11 s | 451 ms | 20x | 9.10 s to 9.23 s | 443 ms to 534 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 16.51 s | 430 ms | 38x | 16.48 s to 16.75 s | 420 ms to 439 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 9.22 s | 556 ms | 17x | 9.20 s to 9.23 s | 543 ms to 649 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 16.76 s | 452 ms | 37x | 16.74 s to 16.80 s | 450 ms to 453 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.54 s | 648 ms | 22x | 14.51 s to 14.73 s | 641 ms to 657 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 72 ms | 18 ms | 4.0x | 72 ms to 72 ms | 17 ms to 33 ms | daemon |  |

#### Cold, Cranelift, Linux x86-64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 71.44 s | 69.88 s | 73.07 s | 72.59 s | 0 | 32,580 MB | 100 MB |

#### Memory and disk, Cranelift, Linux x86-64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 14.1 GB | 3.0 GB | 10.9 GB | 10.5 GB |
| bun 1.4.3 | Edit a function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.5 GB |
| bun 1.4.3 | Edit a widely used function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.4 GB |
| bun 1.4.3 | Change a signature | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.5 GB |
| bun 1.4.3 | Edit a generic function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.4 GB |
| bun 1.4.3 | Add a function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.5 GB |
| bun 1.4.3 | Nothing changed | 56 MB | 25.6 GB | 56 MB | 25.5 GB | 25.5 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 10.5 GB | 25.5 GB | 24.5 GB after 33 builds | −1.0 GB | 14.0 GB, 10.5 GB | 5.7 GB | 6.6 GB | 96 MB | 1 MB |

#### How fast the program runs, Cranelift, Linux x86-64, bun

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 1.40 s (1.40 s to 1.40 s); 108 MB, 591.4 MB | 961 ms (960 ms to 962 ms); 100 MB, 397.9 MB | 136 ms (136 ms to 136 ms); 100 MB, 314.0 MB |

#### Edits, LLVM, Linux x86-64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | blaze | | cargo runs | blaze runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 9.06 s | 462 ms | 20x | 9.04 s to 9.14 s | 455 ms to 536 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 16.50 s | 438 ms | 38x | 16.49 s to 16.55 s | 437 ms to 439 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 9.16 s | 526 ms | 17x | 9.13 s to 9.21 s | 525 ms to 618 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 16.86 s | 454 ms | 37x | 16.71 s to 16.95 s | 453 ms to 465 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.49 s | 617 ms | 23x | 14.41 s to 14.75 s | 614 ms to 630 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 72 ms | 26 ms | 2.8x | 72 ms to 74 ms | 23 ms to 26 ms | daemon |  |

#### Cold, LLVM, Linux x86-64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `blaze build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `blaze build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 78.33 s | 71.93 s | 74.95 s | 73.11 s | 0 | 31,245 MB | 68 MB |

#### Memory and disk, LLVM, Linux x86-64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). blaze is the daemon, its workers and what they run, and the `blaze build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | blaze, peak | cargo, largest process | blaze, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 13.9 GB | 3.0 GB | 10.6 GB | 10.3 GB |
| bun 1.4.3 | Edit a function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Edit a widely used function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Change a signature | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Edit a generic function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Add a function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.2 GB |
| bun 1.4.3 | Nothing changed | 100 MB | 25.2 GB | 100 MB | 24.9 GB | 25.1 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | blaze target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 10.3 GB | 25.1 GB | 24.2 GB after 33 builds | −952 MB | 13.8 GB, 10.4 GB | 5.7 GB | 6.5 GB | 64 MB | 1 MB |

#### How fast the program runs, LLVM, Linux x86-64, bun

The same command run by the program blaze linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | blaze | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 916 ms (916 ms to 917 ms); 135 MB, 530.5 MB | 961 ms (961 ms to 962 ms); 135 MB, 397.9 MB | 135 ms (135 ms to 136 ms); 135 MB, 314.0 MB |

<!-- bench:end -->
