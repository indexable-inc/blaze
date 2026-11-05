# flecs-c

- Upstream: https://github.com/indexable-inc/flecs-opinionated (`crates/flecs-c`), git 808dca8f0d
  ("flecs-c: a native-only rest feature"): `src/`, `csrc/`, `wasm-libc/` and `build.rs`.
- Flecs itself is not copied in. build.rs compiles the amalgamation (`distr/flecs.{c,h}`) of the
  [indexable-inc/flecs](https://github.com/indexable-inc/flecs) fork, branch `indexable/v4.2`,
  through its `flecs-distr` crate, a git build-dependency pinned by rev in Cargo.toml: upstream
  v4.2.0 plus the fork's fixes, among them the REST server's `readonly` that `glue_rest_new` sets.
  The rev is harness's (`crates/harness/vendor/hyperion-flecs-c/Cargo.toml`), and
  `.github/scripts/flecs_pin.py` (blaze.yaml's `changes` job) fails when the two differ.
- Licence: MIT, flecs-opinionated's (LICENSE) and Flecs's (the fork's LICENSE).
- Left out: the `host` feature's optional `flecs_ecs_sys` git dependency (a lock records
  optional dependencies whatever the features are, and blaze links no other Flecs).

Changes, each marked `blaze:` in build.rs:

1. Flecs's amalgamation comes from `flecs-distr` (`flecs_distr::DIR`), not from flecs-opinionated's
   `vendor/flecs/distr`.

To move flecs: commit to the fork's branch, then set the same `rev` here and in harness's
`hyperion-flecs-c/Cargo.toml` in one change. glue.c's struct sizes are checked against Rust's by
this crate's tests (`cargo test -p flecs-c --features rest`), and blaze-web's tests run the REST
world. To take a newer flecs-opinionated `crates/flecs-c`: copy it over, take back the change above,
and update the commit here.
