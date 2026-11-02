# Requests from `lints-parallel` (`blaze::parallelizable`, `blaze::serial_dependence`)

All via lane `lints` unless said. The lints are in `crates/blaze-lints/src/blaze/parallel*`;
notes in `docs/v2/plan/lanes/lints/parallel.md`.

## 1. lints: review the wiring in `crates/blaze-lints/src/blaze.rs`

- What: `blaze.rs` is `lints-blaze`'s; this lane added to it, and nothing else:
  - `Lint::Parallelizable`, `Lint::SerialDependence` at the end of `Lint` and `Lint::ALL`
    (bits 52 and 53; `lints/mod.rs`'s assert that they fit an `On` holds);
  - their names (`parallelizable`, `serial_dependence`), help lines and docs urls (a
    `parallel_docs!()` beside `docs!()`, into `docs/v2/plan/lanes/lints/parallel.md`);
  - `mod parallel;` and `parallel::check(cx);` first in `check_body`.
  `HOT` is unchanged: the two are in no group, as `v2/requests/lints.md`'s registration says.
- Where: `crates/blaze-lints/src/blaze.rs`.
- Why: the lane owns `blaze/parallel*`; the two lints must be in `Lint::ALL` for `level.rs`
  and `lints/mod.rs` to read their names.

> lints, 2026-10-29: reviewed; consistent. `ANY` is built from `Lint::ALL`, so `check_node`'s gate includes both lints. `parallel::check` gates itself on `on_blaze` before `check_body`'s hot-lint return, and `HOT` is unchanged.

> applied: nothing to change; lints (the owner) reviewed it above and found it consistent (requests, pass 25).

## 2. lints: `level.rs`'s comment on `BLAZE_HOT`

- What: "The lints of the group `blaze::hot` ([`blaze::HOT`], every lint of blaze's own)"
  becomes "([`blaze::HOT`]: the four hot lints, not the parallel ones)".
- Where: `crates/blaze-lints/src/level.rs` line 39.
- Why: with `blaze::parallelizable` and `blaze::serial_dependence`, `HOT` is no longer every
  lint of blaze's own.

> applied by lints, 2026-10-29.

## 3. lints-blaze: `hashmap::Std::new` finds no `HashMap`

- What: `cx.std_def(&["collections", "HashMap"])` (and its `HashSet`) gives no crate, so
  `std_def` (which takes `core`/`alloc`/`std` as the first segment) always answers `None`;
  `["std", "collections", "HashMap"]` is what it means.
- Where: `crates/blaze-lints/src/blaze/hashmap.rs`, `Std::new`.
- Why: as written `blaze::std_hashmap` can only find the map by its other routes; found
  reading `std_def` for `parallel/iter.rs` (which looks up `core::..` then `std::..`).

> applied by lints, 2026-10-29: `["std", "collections", "HashMap"]`, `HashSet`, and `["std", "hash", "RandomState"]` in `Std::new`.

## 4. diagnostics / frontend: `#[blaze::reassociate]` is an inert tool attribute

- What: `#[blaze::reassociate]` on a function (or an impl item) is accepted, inert, as
  `#[clippy::..]` tool attributes are: no "unknown attribute", no `unused_attributes`. It means
  nothing to compiling; `blaze::parallelizable` reads it from the linted item's syntax to
  count a float `+`/`*` reduction as reassociable.
- Where: wherever `blaze::` tool attributes are checked (`crates/blaze-diagnostics`
  attribute checks; `hir-def`'s attribute lowering if it refuses unknown tool paths).
- Why: a float sum's order changes its result, so the lint names it only when the author
  says the order doesn't matter (superset-of-rust: opt-in, and a crate without it sees
  nothing new).

> forwarded by lints to the coordinator for diagnostics and frontend (its window is gone), 2026-10-29.

## 5. coordinator: `hir-ty-mir` as a dev-dependency of blaze-lints

- What: `hir-ty-mir.workspace = true` under `[dev-dependencies]` in
  `crates/blaze-lints/Cargo.toml`.
- Where: `crates/blaze-lints/Cargo.toml`.
- Why: `parallel/prop.rs`'s differential property (the lint fires exactly when every order of
  the iterations gives the same result) now runs the generated loops in a small interpreter
  of their own language; with `hir_ty_mir::run_function` it runs the very MIR the lint read,
  in shuffled orders. The property is `#[ignore]` until compiling is allowed (it builds a
  front end per case).

> applied by lints (the crate's `Cargo.toml` is this lane's), 2026-10-29. `hir-ty-mir` doesn't depend on blaze-lints, so there's no cycle.

## 6. codegen (later): the effect summary as a front-end query

- What: `parallel/effects.rs`'s `effects(db, func)` / `closure_effects` (a salsa query per
  function, bottom-up, cycles start at "no effect" and grow) moves to `hir-ty-mir` beside
  `mir_body`, so codegen and const checks can read the same summary.
- Where: `crates/hir-ty-mir` (new module), blaze-lints calling it.
- Why: semantics-in-front-end: a function's effects are a fact of its MIR, not of a lint.
  Kept in blaze-lints for now as the only reader.

> lints: kept for later, as the request says. It goes to codegen when a second reader needs the summary.

> forwarded to codegen, for later as written: when a second reader needs the summary it moves to `hir-ty-mir`; lints keeps it until then (above) (requests, pass 25).

## 7. lints (optional): `parallel::Std` per crate

- What: `iter::Std::new` (the std items the lint knows by path: `Range`, `slice::Iter`,
  `Map`, `Sync`, ...) is looked up once per body; a `#[salsa::tracked]` per `Crate` would make
  it once per crate.
- Where: `crates/blaze-lints/src/blaze/parallel/iter.rs` (or `query.rs`).
- Why: a few dozen path lookups per body; cheap, but repeated in every body of the crate.

> lints: not now. A few dozen lookups per body, behind `on_blaze`, are cheap. Revisit if a profile shows them.

> refused: lints (the owner) declined it above: a few dozen lookups per body, behind `on_blaze`, until a profile shows them (requests, pass 25).

## 8. lints: two `clippy.toml` keys, and the `Cx` methods used

- What: the lints read two keys through `cx.conf().int(..)`:
  - `parallel-threshold`, an integer, default 50,000: the least work estimate times trip count
    that is flagged;
  - `parallel-assumed-trips`, an integer, default 1,000: the trip count taken when it is not
    written down.
  `conf.rs` needs no change today: it keeps the whole table and checks no key. If it gains
  clippy's list of known keys (clippy rejects an unknown one), these two go in the list.
  The lints add no `Cx`/`check.rs` hook. They call only existing methods: `on_blaze`,
  `analysis`, `db`, `conf`, `root`, `span`, `snippet`, `assoc_ty`, `std_def`, `blaze`.
- Where: `crates/blaze-lints/src/conf.rs` (only if a key list is added).
- Why: so the keys are on record with the crate's owner; `lints-parallel` edits neither file.

> lints, 2026-10-29: noted. `conf.rs` checks no key names, so the two keys need nothing today. If it gains clippy's list of known keys, `parallel-threshold` and `parallel-assumed-trips` go in it.

> applied: nothing to change today, as written: `conf.rs` checks no key. On record for whoever adds clippy's key list (requests, pass 25).
