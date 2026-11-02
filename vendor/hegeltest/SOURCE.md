# hegeltest, hegeltest-macros, hegeltest-c

Hegel, the property-based testing library blaze's property tests use (docs/contributing.md,
"Property tests"): Hypothesis's engine ported to Rust, with a Rust frontend.

- Upstream: our fork https://github.com/indexable-inc/hegel (of hegeldev/hegel), commit
  `cae7cfa19251314351738a31234a3b576f2c5421` ("Merge pull request #3 from
  indexable-inc/wasi-backend"), the same commit the ix monorepo pins as its `vendored/hegel`
  submodule. Engine changes go to the fork's `main` by pull request, then come here as a new pin.
- Crates, each from a directory of that commit:
  - `vendor/hegeltest`: `frontends/rust`, crates.io `hegeltest` 0.50.3, lib name `hegel`.
  - `vendor/hegeltest-macros`: `frontends/rust/macros`, `hegeltest-macros` 0.50.3.
  - `vendor/hegeltest-c`: `libhegel`, `hegeltest-c` 0.48.0, lib name `hegel_c`: the native engine.
- Licence: MIT, copyright Antithesis, LLC (LICENSE in each directory, the repository's LICENSE).
- Used only as a dev-dependency, with `default-features = false, features = ["static-engine"]`:
  the engine is linked as a Rust crate, with no Python server and no shared library.
- The three directories are excluded from blaze's workspace (root `Cargo.toml`), so their own
  tests are not blaze's to run; they are built only as dependencies of blaze's tests.

Kept from `tests/`: the files under `tests/embedded/` that the `#[cfg(test)] #[path = ...]`
modules name, unchanged, because `cargo fmt --all` (fmt.yaml) resolves those modules and stops
on a missing file. Left out: the rest of every `tests/` directory (the integration tests and
libhegel's other embedded tests), `examples/`, `benches/`,
`scripts/`, `include/` (the C header), the justfiles, AGENTS.md, CHANGELOG.md, RELEASE.md, the
`[[bin]]` fixtures and the dev-dependencies they need, and the hegel workspace's own
`Cargo.toml` and `Cargo.lock`.

Changes, each marked `blaze:`:

1. `hegeltest` has no build script. Upstream's `build.rs` builds the engine as a cdylib with a
   nested cargo when `static-engine` is off and bakes its directory and version into the runtime
   loader; with `static-engine` it only sets those two variables. The loader
   (`src/ffi/sys/loader.rs`) is left out with it, and `src/ffi/sys.rs` stops with a
   `compile_error!` when `static-engine` is off.
2. `hegeltest-c` builds an rlib only (upstream: `cdylib`, `staticlib`, `rlib`).
3. `hegeltest-c` asks for `toml` 1.1.2 (upstream 1.1.6), the version blaze's lock has; the engine
   uses only `toml::de::{DeTable, DeValue, DeString}` and `toml::Spanned`, which 1.1.2 has.
4. The manifests name `[lints.clippy] too_many_arguments = "allow"` (the hegel workspace's
   lints) in place of `[lints] workspace = true`, and turn off target auto-discovery.
5. `hegeltest-c` asks for `miniz_oxide` 0.9.1 (upstream 0.8), the one `flate2` uses in blaze's
   lock; `compress_to_vec_zlib` and `decompress_to_vec_zlib_with_limit` are the same in 0.9.
   Blobs it compressed before still decode (zlib), though the bytes it writes may differ.
