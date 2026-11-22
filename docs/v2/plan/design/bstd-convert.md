---
status: plan
summary: "blaze-bstd-convert: a sans-IO Rust-to-bstd converter and compiler pass that lowers imperative/blocking/async IO functions and mixed-IO structs into explicit sans-IO state machines plus thin bstd drivers, replaces std and common third-party crates with bstd/blaze-* to drive external dependencies toward zero, and surfaces structured decision questions where judgment is required."
date: 2026-11-22
---
# `blaze-bstd-convert`: std & library converter + sans-IO state-machine compiler

## Why

Moving an existing Rust workspace (e.g. `hyperion`, 407 crates, 1,976 `.rs` files) onto `bstd` and
zero (or near-zero) third-party dependencies has three distinct parts:

1. **Dependency & `std` replacement.** Most crates only use `std` (`fs`, `env`, `time`, `net`,
   `sync`, `collections`, `alloc`) and commodity crates (`serde_json`, `toml`, `bytes`, `blake3`,
   `sha2`, `rand`, `rustc-hash`, `foldhash`, `base64`, `flate2`, `ureq`, `tracing`, `rayon`,
   `pico-args`, `anyhow`, `rkyv`, `hegeltest`) that already have `bstd` or `blaze-*` equivalents.
   A small tail (`rsa`, `aes`, `cfb8`, `sha1`, `heed`, `rusqlite`, `libloading`, `cc`) either stays
   external or needs an explicit architectural choice.
2. **Compiling imperative IO into explicit sans-IO state machines.** Replacing `std::fs::read_to_string`
   with `bstd::fs::Dir::read_all` threads capabilities, but real logic that mixes `env::var`,
   `fs::read_to_string`, `fs::write`, `TcpStream`, `ureq`, `Instant::now`, `thread::sleep`, or
   `.await` inside control flow (`if`, `loop`, `while`, `for`, `?`) should become an explicit
   `#![blaze::sans_io]` state machine (`Effect`, `Event`, `Step`, `State`, `Machine::step`) with
   exact minimal live variables per state variant plus a thin `bstd` driver. Likewise, structs that
   mix IO handles (`TcpStream`, `File`, `Instant`) with a `phase` enum and boolean flags should split
   into a pure core state enum and a thin driver.
3. **Interactive & structured decisions.** Ambiguous choices (replace vs wrap vs keep `rusqlite` or
   `heed`; lift a function to a state machine vs thread capabilities directly; allocator strategy
   `Alloc<'_>` vs `cx.frame()` vs `Heap`; `HashMap` hasher choice; `Mutex` vs channel; custom proc
   macros -> `comptime` reflection; `bindgen`/`cc`/`cmake`/`cxx` -> `blaze::c::import!` /
   `blaze::cpp::import!`) must never be silently guessed (`no-fallback`). The converter surfaces
   grouped, high-leverage `Question`s, records answers in a deterministic JSON `Decisions` file
   (interactively or via agent/CLI), and applies them idempotently.
4. **Proc macros -> `comptime` & C/C++ build scripts -> `blaze::c::import!` / `blaze::cpp::import!`.**
   Custom `#[proc_macro*]` crates are surfaced for migration to `const fn` + `core::mem::type_info`
   / `bstd::comptime` reflection (emitting `src/comptime_migration.rs`). Crates using `bindgen`,
   `cc`, `cmake`, `cxx`, `cxx-build`, `autocxx`, `pkg-config`, or `clang-sys` have their headers,
   include dirs, source files, defines, C/C++ dialect, and `OUT_DIR` include sites extracted from
   `build.rs` / `src/` and are surfaced for migration to `blaze::c::import!` / `blaze::cpp::import!`
   (`src/c_interop_migration.rs`) plus declarative `[package.metadata.blaze.c]` source units
   (`docs/v2/plan/design/c.md`, decisions D174/D176), stripping the C/C++ build-dependencies from
   `Cargo.toml`.

## Chosen design (and losing alternative)

- **Chosen:** One zero-third-party-dep crate `crates/blaze-bstd-convert` built on blaze's own
  `syntax` (full Rust 2024 CST/AST), `blaze-toml` (lossless manifest parser/editor), and
  `blaze-json`/`blaze-data`. The entire analyzer, question planner, CFG+liveness state-machine
  compiler, and AST/manifest rewriter are pure sans-IO functions of `(path -> text)` maps; only the
  thin CLI binary touches the filesystem or stdin/stdout.
- **Losing alternative:** Extending `scripts/src/bin/tobstd.rs` (external `ast-grep` shellouts +
  text patterns). Rejected because `ast-grep` cannot build a CFG, run backward dataflow liveness
  analysis, compile control flow into state enums, parse Cargo manifests cleanly, or run sans-IO.

## Pipeline

```
WorkspaceSnapshot (path -> text)
   │
   ├──► analyze::analyze_workspace
   │      • blaze-toml: workspace members + external dep classification (catalog)
   │      • syntax::SourceFile::parse: std/dep paths, alloc sites, IO effect sites,
   │        intra-crate call graph + transitive effect closure, implicit-state structs, walls
   │
   ├──► questions::build_questions ◄──► Decisions (JSON / interactive CLI)
   │      • Grouped decisions: external deps, IO lifting mode, implicit structs,
   │        allocator strategy, map hasher, sync/locks, formatting
   │
   ├──► sm_compiler::compile_fn / compile_struct
   │      • AST -> Effect CFG (straight-line, if/else, loop/while/for, ?, return)
   │      • Backward liveness fixpoint -> exact minimal live locals per suspension state
   │      • Type inference from annotations, params, effect returns, literals/constructors
   │      • Emit: Effect / Event / Step / State enums + Machine::step + thin bstd driver + sim test
   │
   └──► rewrite::convert_workspace
          • Lossless Cargo.toml edits (drop replaced deps, add bstd + features)
          • Span-checked non-overlapping Rust AST edits (std/deps -> bstd, alloc threading,
            #![no_std], #![blaze::sans_io], lifted state machines)
          • Report (before/after external deps, 0-dep crate count, lifted machines, remaining walls)
```

## Cost budget

- Single-pass parse per `.rs` file (`syntax::SourceFile::parse`), linear in source bytes.
- Liveness fixpoint bounded by `MAX_LIVENESS_ITERS = 64` per function (`bounded-progress`).
- On `hyperion` (407 crates, 1,976 `.rs` files): full workspace analysis under 5 seconds on Mac.

## Proof

1. Unit & round-trip tests in `crates/blaze-bstd-convert/src/tests.rs`:
   - Every emitted sans-IO state machine (straight-line, `if/else`, `loop`/`while`/`for`, `?`,
     multi-effect chains, struct split) is re-parsed by `syntax::SourceFile::parse` with 0 parse
     errors AND verified for minimal liveness (dead locals excluded from later states).
   - End-to-end compile check: generated sans-IO state machines compile cleanly under `rustc`.
   - Manifest & Rust rewrite round-trip tests; Decisions JSON round-trip test (`blaze-json`).
2. Real-project verification (`real-run-first`): run `blaze-bstd-convert` against
   `/Volumes/Projects/indexable-inc/hyperion` (analysis, question generation, state-machine lifting
   of real `hyperion` IO functions & structs, and non-destructive conversion of real crates).
