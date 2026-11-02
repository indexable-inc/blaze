# Requests of `requests-backend-axis` (child of lane `requests`)

The backend axis of the corpus differential (`fastcompile-stencil 4` in
`v2/requests/fastcompile.md`) runs each corpus program with the dev tier forced to the stencil
backend and to Cranelift. The v2 daemon has no setting that says which backend the dev tier
uses. `blaze_codegen::plan::OptLevel::backend` maps opt-level 0 and 1 to `Backend::Cranelift`
with nothing over it, and `blaze_cargo::serve::unit::codegen::policy` ignores the unit's
`profile.codegen_backend`, which `blaze-manifest` already parses from cargo's own
`codegen-backend` profile key. Until the setting below exists, `harness/v2/engine.py` refuses an
`engine.dev_backend` other than `"default"` with an error naming this request. It does not fall
back to the default backend.

The setting is cargo's own (nightly's `-Zcodegen-backend` with `[profile.<name>]
codegen-backend`), not a new environment variable or flag. It goes through the profile, so it
is in `Policy::id` and every function's `Settings` with no extra key. `"stencil"` is a blaze
extension value (superset-of-rust: opt-in, and a program that doesn't name it builds as
before).

## requests-backend-axis 1: codegen: the dev tier's backend in the policy (`crates/blaze-codegen/src/plan.rs`)

What:

1. `pub enum DevBackend { Cranelift, Stencil }` (serde, `Copy`, `Hash`), and
   `Profile::codegen_backend: Option<DevBackend>` and `Override::codegen_backend:
   Option<DevBackend>`, layered by `Policy::layers` as `opt_level` is (profile, then `"*"`,
   then the package, then `build-override` for build-time code; `Origin::Std` keeps
   `SHIPPED_STD`, which leaves it `None`).
2. In `Policy::choose`, with `Some(dev)` for the origin:
   - a function whose level is 0 or 1 is compiled with `dev` (`Backend::Cranelift` or
     `Backend::Stencil`, from `fastcompile-stencil 1`), in `Mode::Output` and `Mode::Live`
     alike;
   - in `Mode::Live`, a function at LLVM's level starts at `dev` (`Choice::first`) and tiers to
     its LLVM spec (`then`) as today;
   - no function tiers from one dev backend to the other: a stencil function's tier-up to
     Cranelift (`blaze_stencil::tier`) is off for every function the setting covers, so each
     dev-tier function of the build runs `dev`'s code and nothing else's.
   - `fold` stays as today: on only when the backend is `Backend::Cranelift`.
   `None` keeps today's choice (and stencil tier 0 of live runs once fastcompile lands it).
3. A test beside `plan.rs`'s: `Some(Stencil)` and `Some(Cranelift)` give that backend for a
   member, a dependency and build-time code at level 0, leave std at LLVM `-O3`, give `first`
   `dev` and `then` LLVM for a live function at level 3, and two policies that differ only
   in it have different `Policy::id`s.

Owner: codegen (`crates/blaze-codegen/`, docs/v2/plan/reference/crates.md).

> forwarded to codegen (window `codegen-next1`, told by tmux).

## requests-backend-axis 2: cargo: read `codegen-backend` into the policy (`crates/blaze-cargo/src/serve/unit/codegen.rs`)

What: `policy` reads the root unit's `profile.codegen_backend` into
`Profile::codegen_backend`, and each dependency's that differs into
`Override::codegen_backend`, as it does `opt_level`:

- `"cranelift"` is `Some(DevBackend::Cranelift)` and `"stencil"` is `Some(DevBackend::Stencil)`;
- `"llvm"`, rustc's default backend, is `None`, because which machine code blaze makes for
  the default is its own choice (nobody can tell);
- any other name is an error naming the profile, the package and the value, for example
  ``the profile `dev` of `foo` asks for codegen-backend `gcc`, which blaze has none of
  (`cranelift`, `stencil`, `llvm`)``. Today the key is ignored without a word, which is a
  quiet fallback.

Owner: cargo (`crates/blaze-cargo/` but store's and usercode's paths).

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-exec): half done at `crates/blaze-cargo/src/serve/unit/codegen.rs:177` (`codegen_backend`, called at `:387,399`, called for the root and each dependency in `policy`): the key is no longer ignored. `llvm` and none leave blaze's choice; `cranelift` is taken where the level is Cranelift's already; `stencil`, `cranelift` at an LLVM level and any other name stop naming the profile, package and value. Mapping into `Profile`/`Override::codegen_backend` waits on codegen (requests-backend-axis 1: no `DevBackend` in `crates/blaze-codegen/src/plan.rs`). Tests: `serve/unit/codegen/tests.rs`.

> rechecked by lane cargo (cargo-inbox-exec, 23:35): `Backend::Stencil` is in, `DevBackend` still is not (`crates/blaze-codegen/src/plan.rs` has no `codegen_backend` in `Profile`/`Override`). Unchanged: half done at `crates/blaze-cargo/src/serve/unit/codegen.rs` `codegen_backend`.

## requests-backend-axis 3: coordinator: the `codegen-backend` gate as cargo has it (`crates/blaze-manifest/src/profiles.rs`)

What: `validate_profile_layer` takes `codegen-backend` when the build has `-Zcodegen-backend`
(`Invocation::unstable`, which `crates/blaze-cargo/src/cargo/cli.rs` already collects) or
`[unstable] codegen-backend = true` in the configuration, as nightly cargo does
(`cli_unstable.codegen_backend` beside the manifest's `cargo-features`). Today only the
manifest's `cargo-features = ["codegen-backend"]` opens it (`profiles.rs`, "blaze does not
read those"). The other `-Z` gates can follow the same path, but this request is only for
this one.

Why: the harness sets the backend from the command line (`-Zcodegen-backend --config
'profile.dev.codegen-backend="stencil"'`, see `harness/v2/engine.py`, `dev_backend_args`). It
cannot add `cargo-features` to a corpus crate's manifest without changing the tree that
cargo, the reference, builds.

Owner: coordinator (`blaze-manifest` is one of the cargo crates crates.md leaves to the
coordinator).

> forwarded to the coordinator: `blaze-manifest` is its (listed in requests' report).

## When 1 to 3 are applied

Delete the refusal `DEV_BACKEND_MISSING` in `harness/v2/engine.py` (`Engine.__init__`). The
requests lane holds `harness/v2/` now that proof is done. `V2.build` passes
`dev_backend_args` already, so nothing else changes.
