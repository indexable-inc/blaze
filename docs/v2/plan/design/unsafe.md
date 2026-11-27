---
status: plan
summary: blaze v2: unsafe where it makes code measurably faster, safe Rust everywhere else, and every unsafe line proved sound by our own tools (a Miri of our own, sanitizers in our code generator, model checking). Built: the SAFETY lints and model checking (crates/blaze-check); the UB checker, the sanitizers and the fuzz targets are not built.
date: 2026-10-29
---
# Unsafe: where it pays, and proved

v2 is as fast as Rust allows ([principles.md](../../plan.md)). Sometimes that takes
`unsafe`: mapped pages read in place, lock-free tables, arenas, io_uring buffers, SIMD,
the linker writing an image in place. Everywhere else the code is safe Rust. Unsafe code has to
be proved, not just reviewed.

## Where unsafe is allowed

- **Only where it buys measured speed or is the only way** to use the kernel or the hardware. Each
  use names what the safe version would cost.
- **Behind a safe API.** Unsafe code lives in small modules (`raw.rs`, `mapped.rs`, `simd/`,
  `ring/`) whose public functions are safe and can't be misused from safe code. A crate with no
  such module says `#![forbid(unsafe_code)]`.
- **Every `unsafe` block has a `// SAFETY:` comment** saying which invariant holds, where it is
  upheld, and what breaks if it isn't. `unsafe fn`s carry a `# Safety` section. The workspace
  lints deny `unsafe_op_in_unsafe_fn` and `clippy::undocumented_unsafe_blocks` and warn on
  `clippy::multiple_unsafe_ops_per_block`.
- **Bytes from disk or the network are untrusted until verified.** Before any zero-copy view is
  made of them, their content hash is checked against the key, and offsets and lengths are
  bounds-checked against the mapping. Nothing is transmuted unchecked from the store, the
  remote cache or an object file.

## How it is proved (all without rustc)

- **Our own Miri.** blaze already interprets bodies for const eval (blaze-body-eval). It
  becomes a checker of undefined behavior for test runs:
  - Tree Borrows aliasing, alignment, uninitialized reads, out-of-bounds offsets, invalid
    values, use after free and leaks;
  - data races, detected with vector clocks over the interleavings it picks.

  `blaze test --ub` runs each unsafe module's tests under it. This is a superset feature:
  opt-in, and Rust's own behavior is unchanged.
- **Sanitizers from our code generator.** LIR's code generator emits AddressSanitizer,
  ThreadSanitizer, MemorySanitizer and SanitizerCoverage instrumentation. This turns today's
  refusal of `-Zsanitizer` (`docs/support.md`) into support, which is superset-of-rust: rustc's sanitizer flags work under blaze.
- **Model checking of concurrency.** Every lock-free structure is tested under blaze-check
  ([model-checker.md](model-checker.md)), a loom-style scheduler (built). It explores the
  interleavings and memory orderings (relaxed, acquire, release) up to a bound, against a
  sequential model.
- **Fuzzing.** Coverage-guided fuzz targets, built by blaze with SanitizerCoverage and ASan, for
  every parser of untrusted bytes: store packs and indexes, the remote protocol, CBOR, object
  files and archives in the linker, and wasm proc macros.
- **Differential.** Each unsafe fast path keeps a safe reference version in tests, and property
  tests check that both give the same bytes.

## Done when

- `rg 'unsafe \{'` finds no block without `// SAFETY:` in v2's crates, and every crate without
  an unsafe module forbids `unsafe_code`.
- Every unsafe module's tests pass under `blaze test --ub` and under ASan and TSan.
- Every lock-free structure passes model checking, and every untrusted-bytes parser has a fuzz
  target that has run its time budget in CI with no finding.
