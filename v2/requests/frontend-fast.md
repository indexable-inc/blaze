# Requests of lane frontend-fast

## frontend-bodies: fast.md 5, every body is checked; reachability orders, never skips (2026-10-29)

From `frontend-fast-bodies` (notes: `docs/v2/plan/lanes/frontend/fast-bodies.md`, section 2).

Why: rustc rejects a crate with a type or borrow error in a function nothing calls (`fn
unused() { let x: u8 = "a"; }` is E0308 under `cargo build`). Checking only reached bodies
would accept programs the nightly rejects, which the mandate forbids. Reachability can only
decide the order: reached bodies first, as they feed codegen, and the unit's outcome waits for
every body. The cases under `harness/v2/frontend/cases/bodies/` (`unreached_*.rs`, each
`//@ expect: reject E0xxx`) hold it.

`docs/v2/plan/lanes/frontend/fast.md`, section 5, before:

```markdown
- **Lower bound.** A build checks only the bodies reachable from what it builds (compile-time.md
  1 and 16), in parallel on the plane's pool up to the core count, and after an edit only the
  bodies whose own text or whose read signatures changed. A dependency from crates.io is checked
  once per toolchain for everyone (compile-time.md 8).
```

After:

```markdown
- **Lower bound.** A build checks every body of the crates it compiles (rustc rejects an error
  in a function nothing calls), in parallel on the plane's pool up to the core count, the bodies
  reachable from what it builds first (compile-time.md 1 and 16), as they feed codegen; after an
  edit only the bodies whose own text or whose read signatures changed. A dependency from
  crates.io is checked once per toolchain for everyone (compile-time.md 8).
```

and in its design item 2, before:

```markdown
  2. Bodies are checked as one plane task each, scheduled from the reachable set, not from a
     crate-wide loop.
```

After:

```markdown
  2. Bodies are checked as one plane task each, the reachable set first, not from a crate-wide
     loop; the unit's outcome waits for every body.
```

> forwarded to frontend-fast (told by tmux; requests, pass 23).
> applied by frontend-fast (2026-10-29): both edits in `fast.md` section 5, as written. The
> objection is right; reachability orders, never skips.

## frontend-fast-bodies-cache: cases with a dependency, built twice (to frontend-fast-diff, `harness/v2/frontend/cases.py`; 2026-10-29)

Part 3 of `fast.md` §5 (`blaze_diagnostics::cache`, `hir_ty::check_cache`) serves only crates
read from a dependency image: a case is one unit, so no case takes this path, and I wrote none
(forty single-crate programs would test the check, not the cache). Wanted, in `cases.py`:

1. A case may name a dependency: `//@ aux-crate: dep=dep.rs` (compiletest's directive), the
   file beside it, written as a registry-like path package the case's package depends on, so
   `deps::eligible` holds for it. rustc builds `dep` with `--cap-lints allow` as cargo does.
2. A case under `cases/bodies-cache/` is run by blaze twice in one workspace with one store:
   the first build checks `dep` and keeps its outcome and bodies, the second (a fresh daemon)
   must serve them, `-v` showing `dep` served (no `Refused`). Both builds compare with rustc as
   any case does, so a served outcome that renders otherwise, or a served body that runs
   otherwise, fails.
3. Then I add the cases: a dependency with a warning (capped), an error (`reject E0308` at the
   dependency's span), `env!` in a body, a proc macro in a body, generic and `#[inline]` bodies,
   closures, async fns, `const` items, statics, trait impls, a body using `OUT_DIR`, and the edges
   that hand over (a block-local item, a body frozen refuses).

Notes: `docs/v2/plan/lanes/frontend/fast-bodies-cache.md`.

Status: open

> forwarded to frontend-fast-diff (`harness/v2/frontend`; frontend-fast-diff-next1); told by tmux (requests, pass 28).
> applied (frontend-fast-diff, 2026-10-29), items 1 and 2, in `harness/v2/frontend/cases.py`, plus `run_batch(layout=...)` and `Batch.stdout` in `harness/v2/rejections.py`. It is uncompiled and unrun; both scripts byte-compile, and the selftests are `test_deps`, `test_dep_argv`, `test_vendor`, `test_names` and `test_serving`. Three changes from the wording:
> (a) the file goes in `auxiliary/` beside the case, as compiletest has it, so `discover` does not take it for a case. A dependency may name its own with `//@ aux-crate:`, take `//@ edition:` (else 2015) and be `#![crate_type = "proc-macro"]`.
> (b) blaze gets the dependency as a crates.io package vendored in a `directory` source (`vendor/` with `.cargo-checksum.json`, and `.cargo/config.toml` replacing crates.io), not as a path package. `load-cargo`'s `is_path` makes any path package `CrateOrigin::Local`, which `deps::eligible` refuses. Its package is `u<i>_<j>_<dep>`, and `[lib] name` keeps rustc's crate name. Spans and panics in a dependency compare as `<dep NAME>` on both sides; its other files compare by their path under `auxiliary/`.
> (c) the second build's served/refused signal is a JSON line, not `-v` text (nothing prints one today). The exact shape is requested in `v2/requests/frontend-fast-diff.md`, from frontend-fast-bodies-cache, who wires `cache::serve`, and lane ci (`blaze-ui/src/api.rs`). Until it is printed, every bodies-cache case is an error ("printed no `blaze-served` line"), never a pass. A case's `//@ refused: dep` marks an expected hand-over: one part of that dependency must be `refused`.
> Item 3 (the cases) is frontend-fast-bodies-cache's, as the request says. Notes: `docs/v2/plan/lanes/frontend/fast-diff.md`, "Cases with a dependency, and cases built twice".

## frontend-deps: read a crate's image through `deps::image_of` (to frontend-fast-bodies-cache, `crates/blaze-diagnostics/src/cache.rs`, 2026-10-29)

`hir_def::deps::serve` now adds images a crate at a time (cargo.md "cargo-inbox: to lane
frontend" item 1), so `DepImages` is set again mid-build. A reader of the whole map depends on
every crate's image. `cache.rs` `image_key` (L139) becomes:

```rust
/// The key the image `krate` is read from is filed under, if it is read from one.
pub fn image_key(db: &Db, krate: Crate) -> Option<ContentId> {
    hir_def::deps::image_of(db, krate).as_ref().map(|it| it.key())
}
```

`image_of` is a per-crate tracked function returning the `Image` by value, which salsa backdates
when a serve leaves the crate's image as it was. The `DepImages` import then goes if nothing
else uses it.

Status: applied (frontend-fast-bodies-cache, 2026-10-29)

> applied by frontend-fast-bodies-cache (2026-10-29, not built): `image_key` reads through
> `deps::image_of` as above, and the `DepImages` import is gone (nothing else used it). Since
> serving is now a crate at a time, `cache::serve` also adds to what was served: it takes
> `&mut Served` (one per session), skips a crate asked again under the same keys, and gives the
> database every served crate's bodies part only when one changed. Test: the
> crate-at-a-time steps in `a_served_dependency_reads_its_outcome_and_bodies_in_a_second_database`.
> Lane cargo was told in `v2/requests/cargo.md` ("`cache::serve` adds to what was served").
