# Requests of fastcompile-macros-corpus

- (done: all in `[dev-dependencies]`) To the parent, fastcompile-macros, "fastcompile-macros-corpus 1": `crates/blaze-macro-eval/Cargo.toml`
  `[dev-dependencies]`, each `workspace = true`: `native-proc-macro`, `blaze-macro-wasm`,
  `proc-macro-api`, `blaze-units`, `blaze-query`, `blaze-vfs`, `serde` (with
  `features = ["derive"]`), `serde_json`. `tests/corpus.rs` and `tests/kept.rs` use them (the
  in-test reference, the wasm and toolchain references, planning and lowering the fixture
  workspace, the typed `BLAZE_MACRO_CORPUS` config). Everything else they use is a dependency
  already.
- (filed as "fastcompile-macros 6" to lane proof in `v2/requests/fastcompile.md`) To the parent, "fastcompile-macros-corpus 2": a harness script (`harness/macroeval_corpus.py`,
  one JSON value in, as rule 17 says) that, in an ix VM: copies
  `crates/blaze-macro-eval/tests/corpus/ws` twice (`workspace`, `scratch`); fetches its
  registry packages with `cargo fetch --locked` into a `CARGO_HOME` of its own; builds each copy
  with `blaze build -p corpus-calls` (std's and every build script's runs left beside their
  `OUT_DIR`); builds rustc's library of each proc-macro crate (`serde_derive`,
  `thiserror_impl`, `tokio_macros`, `corpus_macros`, `corpus_state`) with the pinned nightly as
  the reference, and the toolchain's `proc-macro-srv`; optionally blaze's wasm module of each;
  writes the `BLAZE_MACRO_CORPUS` config (`tests/corpus/config.rs`) and runs
  `blaze test -p blaze-macro-eval --test corpus --test kept -- --ignored`, with `BLAZE_CACHE`
  and `BLAZE_PROC_MACRO_CACHE` unset.
