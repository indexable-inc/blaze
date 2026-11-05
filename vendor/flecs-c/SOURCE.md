# flecs-c

- Upstream: https://github.com/indexable-inc/flecs-opinionated (`crates/flecs-c`), git 808dca8f0d
  ("flecs-c: a native-only rest feature"). `flecs/flecs.c` and `flecs/flecs.h` are that repository's
  `vendor/flecs/distr`, Flecs C's own amalgamation at
  [`andrewgazelka/flecs@b18f84ad0`](https://github.com/andrewgazelka/flecs/commit/b18f84ad03ae02e42830e32634d65b7bcbadeafd):
  upstream SanderMertens/flecs master `72a531d6f` plus four fixes open as upstream PRs (#2347,
  #2348, #2349, #2350; the last is the REST server's `readonly`, which `glue_rest_new` sets).
- Licence: MIT, flecs-opinionated's (LICENSE) and Flecs's (flecs/LICENSE).
- Left out: the `host` feature and its optional `flecs_ecs_sys` git dependency (a lock records
  optional dependencies whatever the features are, and blaze links no other Flecs).

Changes, each marked `blaze:` in build.rs:

1. The amalgamation is looked for in the crate's own `flecs/`, so that the crate is whole where it
   is published.

To move the pin: copy `crates/flecs-c` and `vendor/flecs/distr` of a newer flecs-opinionated, take
back the change above, and update the commits here.
