# Research

Findings, one file per topic, dated, each claim marked **read** (in a source we read, or
measured), **reported** (someone says so) or **unverified** (our inference). What was fetched to
back them is under [docs/upstream/](../upstream/), each with a `SOURCE.md`. Search here first.

- [rust-compiler-2026.md](rust-compiler-2026.md): the Rust compiler's 2026 (next solver,
  Polonius, LLVM 23, move elimination, polymorphization, cargo's build-dir) and what ra-jit
  does about each. 2026-10-20.
- [mir-gap.md](mir-gap.md): ra-jit's MIR against rustc's optimized MIR, match rates, the
  passes ported and not, and the production gaps ranked. 2026-10-20.
- [glancer.md](glancer.md): Rust Glancer (frozen, offloaded per-package index, phase-wise memory
  discipline), matklad's three tiers (syntax trees, stubs, `.rmeta`-like dependencies), what
  rustc's `.rmeta` keeps MIR for, and salsa 0.28's persistence. 2026-10-21.
- [daemon-memory.md](daemon-memory.md): what the daemon's memory is (by salsa query and
  allocation site, ripgrep and nushell), the spare worker's share, and what salsa 0.28.5's
  `lru` can and cannot evict (not a query with cycle handling, only at a new revision).
  2026-10-21. A spare only for a build that asks, salsa patched to evict queries with cycle
  handling and what that costs edits, and a forced `mi_collect`. 2026-10-22. What the own
  base adds after ready (`described` first), the daemon's tree process by process, and
  mimalloc against jemalloc. 2026-10-23. Sharing `described`'s types: nushell, helix, bun
  and zed before and after, and how the DWARF was found the same. 2026-10-23.
- [disk.md](disk.md): what ra-jit keeps on disk against cargo on ripgrep, helix, nushell and
  bevy, part by part with reflinked blocks counted once, the store before and after zstd
  blocks, what is left after the no-rustc lane (F/G) projected, the splice's one copy of the
  debug sections on ext4, the daemon's program against rustc's section by section, and which
  compressed debug information std, gdb, lldb and addr2line read (off). 2026-10-23.
- [proc-macro-cache.md](proc-macro-cache.md): rustc's `-Zcache-proc-macros` (derives only, a
  query per expansion), the earlier attempts, and what ra-jit's expansion cache keys and why
  spans are in the key. 2026-10-21.
- [borrowck.md](borrowck.md): rustc's NLL borrow checker and Polonius (the nightly-default
  alpha, the legacy datalog analysis, stabilization status, the UI tests that tell them apart),
  rust-analyzer's removed borrowck, and what ra-jit builds. 2026-10-20.
- [bun.md](bun.md): bun's Rust port (1.4.3, 1.25M lines in 103 crates on a nightly) built
  by ra-jit: how the harness builds it, what broke and was fixed, what is filed, and its edit
  and runtime numbers against cargo; why the whole program ran 5.9x slower than release and
  the added function took 19.8 s, and the macOS whole-program link. 2026-10-21, 2026-10-23.
- [codegen-units.md](codegen-units.md): whole-program builds compiled many functions to an
  LLVM module (`RA_JIT_UNITS=1`, #176): how the functions are found and cut, what units kept of
  their imports, the build and run times on ripgrep, helix, nushell and bun against compiles
  of one function and cargo's release build, and why LLVM takes four times rustc's time for
  code of the same size. 2026-10-24.
- [../architecture-split.md](../architecture-split.md): what of ra-jit belongs in the
  rust-analyzer subtree and what in `crates/`; ra-jit's changes to rust-analyzer by purpose,
  upstream's churn on the files they touch, rustc's crate split, and a migration plan.
  2026-10-21.
