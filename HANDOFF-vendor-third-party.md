# Handoff: lane T2 `vendor-third-party` (docs/v2/plan/design/no-tracing.md, 6.2)

Branch `nt-vendor-third-party`. I wrote code only: no build, check, test, cargo, rustc, blaze or
VM. I did run `tools/vendor/sync.py`, which is plain Python. It fetched the four crates, checked
them, replayed the patches and wrote the trees. Then `"check": true` confirmed each
`vendor/<crate>` is byte for byte upstream plus its series. I also checked by hand that a broken
hunk, a leftover `tracing` site, an unknown version and an unknown input field each stop the
script with an error that names them.

## Upstream versions (from Cargo.lock; each `.crate` checked against it)

| crate | version | sha256 |
|---|---|---|
| quinn | 0.11.12 | `4051e23e9185c255a7e33ef59cdbca87a22d359052eecd22fc6b901fb37d9d11` |
| quinn-proto | 0.11.19 | `0e750cca55fe4f0439a15d0bb529da9651e79993e8e72c61a899a36d462befbe` |
| wtransport | 0.7.2 | `b4273ce3157a3262a68665f8d3f20a0ac0c5b8a69ffd67f05ae986832ebec036` |
| ra-ap-rustc_pattern_analysis | 0.179.0 | `978b57ee433aec2cd5775546ade5cc04d84c060240d1e3486cce79736e46841c` |
| quinn-udp (no copy) | 0.5.16 | `af66907df18639dcf4db56ca65490cabc4b27a97dbadd96f2926cca73298f016` |

## Files changed

- `vendor/{quinn,quinn-proto,wtransport,ra-ap-rustc_pattern_analysis}/` (new). Each is the
  published `.crate` minus what its `Left out:` line names, with the series applied. Beside it
  are `PATCHES.md` (the `Base:` and `Left out:` lines, plus what and why for each patch) and
  `patches/`. This follows `vendor/rustc_next_trait_solver`, except the patches cover the whole
  crate (manifest and tests too), not just `src/`. `Cargo.toml.orig` is upstream's, never patched.
- Series:
  - every crate: `0001-no-tracing`;
  - quinn and wtransport: also `0002-no-examples-or-benches` and `0003-tests-use-vendored-siblings`;
  - pattern_analysis: also `0002-tests-name-the-published-crate`.
- `tools/vendor/sync.py` (new). It takes one typed JSON value, `{crate, version, check?, root?}`,
  and uses asyncio, with the index and lock lookups run in a TaskGroup. Steps:
  1. reads the checksum from the crates.io sparse index (a yanked version is refused) and checks
     it against `Cargo.lock` when the lock has that version;
  2. downloads the `.crate` into a new, empty temp directory, checks its SHA-256, and extracts it
     with tarfile's `data` filter, refusing links and any path outside `<crate>-<version>/`;
  3. replays the patches with `git apply`, exact context and no fuzz. On failure it names the
     patch and each failing hunk (file and `@@` line), or says upstream already has the patch;
  4. refuses a replayed tree that still names `tracing` or has a span or log macro, naming file
     and line;
  5. only then writes the tree, rewrites each patch against the new base, and updates `Base:`.

  `check` writes nothing and fails on any difference. The sync keeps a gate `Cargo.lock` in
  `vendor/<crate>/` if one is there.
- `Cargo.toml` (root, integrator-owned; edited because the brief asked):
  - the four crates are added to `exclude`;
  - `[patch.'crates-io']` gets `quinn`, `quinn-proto`, `wtransport` and
    `ra-ap-rustc_pattern_analysis = { path = "vendor/<crate>" }`.
- `pyproject.toml`: `tools/vendor` is added to ruff's and pyright's `include`.
- `docs/v2/plan/design/no-tracing.md`: the spec, already staged in the worktree, committed with
  this lane.

## What `0001-no-tracing` deletes

`warn!` and `error!` are deleted too: blaze does not record third-party logs (5.1).

- **quinn-proto**:
  - about 200 log statements;
  - the spans `send` (fields `space`, `pn`; it was held in `PacketBuilder::_span`), `first recv`,
    `recv` (`space`, `pn`) and `frame` (`ty`);
  - the `Drop` warning of `IncomingImproperDropWarner` (the zero-sized type stays);
  - a frame-kind `match` that only logged;
  - in tests: `subscribe()`/`TestWriter`, the `info_span!` guards in `Pair`, and the `info!` lines;
  - in the manifest: the `tracing` dependency, the `tracing-subscriber` dev-dependency, and the
    `tracing/log` entry of `log` (the feature stays, empty).
- **quinn**:
  - the log statements, and the `debug_span!("drive", id)` that `ConnectionDriver` held;
  - two `.instrument(Span::current())` calls;
  - the `lock_tracking` warnings;
  - in tests: the subscribers, the `error_span!` guards, `.instrument(error_span!(..))` and `info!`;
  - in the manifest: `tracing`, `tracing-subscriber` and `tracing-futures`;
  - `log` becomes `["proto/log"]`, without `tracing/log` or `udp/log`;
  - the `udp` dependency loses `features = ["tracing"]`.
- **wtransport**:
  - the log statements;
  - `.instrument(debug_span!("Driver", quic_id))` and the two `.instrument(debug_span!("Stream", id))`;
  - `#[instrument]` on `handle_bi_h3_stream`;
  - in the manifest: `tracing` and `tracing-subscriber`.
- **pattern_analysis**:
  - the `debug!` lines and three debug-level `#[instrument]`s (`ret` and argument fields dropped);
  - in tests: `init_tracing`;
  - in the manifest: `tracing`, `tracing-subscriber` and `tracing-tree`.

Span fields dropped: `space`, `pn`, `ty` (quinn-proto); `id` (quinn); `quic_id`, `id` (wtransport);
the instrumented arguments and `ret` (pattern_analysis).

Behaviour notes:
- Where upstream only logged an error, the error is now discarded: quinn's driver I/O errors and
  the `set_only_v6` failure, and quinn-proto's qlog emit failure.
- A disabled `tracing` site evaluates nothing, so deleting it matches running with no subscriber.

## quinn-udp

There is no copy. Its `tracing` is optional, but quinn turned it on twice: `features = ["tracing"]`
on the `udp` dependency, and `udp/log` in the `log` feature, since quinn-udp's
`log = ["tracing/log"]` turns `tracing` on. quinn's `0001` removes both, and nothing else in the
root lock depends on quinn-udp.

## Gate: each crate's own tests under blaze

Each crate is excluded from blaze's workspace, so `blaze test -p quinn-proto` from the root cannot
build its tests: their dev-dependencies are not in blaze's lock, which is the point. Instead, run
`blaze test` inside the crate's directory, which makes it its own workspace:

```
blaze -C vendor/quinn-proto test                  # src/tests/ and the unit tests
blaze -C vendor/quinn test                        # src/tests.rs, tests/ (post_quantum needs aws-lc-rs and is skipped by required-features; many_connections is #[ignore])
blaze -C vendor/wtransport test                   # unit tests in driver/utils.rs and tls.rs, plus doc tests
blaze -C vendor/ra-ap-rustc_pattern_analysis test # tests/{complexity,exhaustiveness,intersection}.rs
```

(or `cd vendor/<crate> && blaze test`). Patch `0003` points quinn and wtransport at the vendored
siblings. Run this on every bump, after `uv run --frozen tools/vendor/sync.py '{"crate": ..., "version": ...}'`.

## Expected Cargo.lock changes (left to the integrator)

- `quinn` 0.11.12, `quinn-proto` 0.11.19, `wtransport` 0.7.2 and `ra-ap-rustc_pattern_analysis`
  0.179.0 lose their `source` and `checksum` lines (they become path packages) and drop `tracing`
  from `dependencies`.
- `quinn-udp` 0.5.16 drops `tracing` from `dependencies`; its source and checksum stay.
- The `tracing` package stays: blaze's own crates and hyper-util still use it until T1 and phase 2.
- `tools/nix/units/daemon.json` embeds the lock and needs regenerating the same way.
- Each gate run writes `vendor/<crate>/Cargo.lock`. I recommend committing those after the first
  green run, so the gate always runs on the same tree; the sync keeps them. They should contain
  no `tracing`.

## Risks

1. Unverified build. No compiler has seen these trees. I swept for leftover `tracing` sites
   (script step 4), empty blocks and bindings that became unused, and fixed each one I found
   (`Err(_)`, `let _ =`). An unused-variable warning or a missed shape could still remain.
2. Do blaze and cargo resolve these the way I assume? Three things are unconfirmed:
   - (a) that `blaze -C vendor/<crate> test` treats an excluded directory as its own workspace,
     as cargo does;
   - (b) that blaze honours `[patch.crates-io]` in that standalone manifest (`0003`);
   - (c) that blaze ignores that `[patch]` table quietly when the crate is a dependency of
     blaze's workspace, as cargo does.

   If (a) fails, give each crate an empty `[workspace]` table in a new patch. If (b) fails, the
   gate tests against crates.io siblings.
3. pattern_analysis's published tests never compiled as shipped: they name
   `rustc_pattern_analysis`, but the crate is published as `ra_ap_rustc_pattern_analysis`. Patch
   `0002` adds an `extern crate` alias. They may have other publish-time breakage I could not see
   without building.
4. Front-end code identity: `ra-ap-rustc_pattern_analysis` goes from a registry package to a
   path package in hir-ty-matchck's closure. `blaze-codegen-code`'s walk and the
   `front_end_identity` test in blaze-cargo should cover it by file content. Check that it is
   neither missed (`keys-cover-every-input`) nor refused.
5. Other workspaces:
   - `crates/blaze-wasm` has its own workspace and lock, with ra-ap-rustc_pattern_analysis 0.179.0
     from crates.io. Its `[patch.crates-io]` copies the root's, so it needs
     `ra-ap-rustc_pattern_analysis = { path = "../../vendor/ra-ap-rustc_pattern_analysis" }`,
     which is not this lane's file.
   - `crates/blaze-wasm/web/dioxus/Cargo.lock` has quinn, quinn-proto and quinn-udp with
     `tracing`, via dioxus. The spec's table does not cover it; the lockfile ban will flag it if
     it scans that lock.
6. Upkeep: quinn-proto's `0001` is about 3000 lines. Upstream changes often hit its hunks, and
   the sync names exactly which ones.

## What the main agent should check

- Run the gate commands above (four `blaze test` runs), keep going, and commit the gate locks.
- Build blaze with the new `[patch]` entries and confirm the lock changes match the list above.
  Then `blaze check`, focused on blaze-explore (wtransport) and hir-ty-matchck (pattern_analysis).
- `uv run --frozen ruff check` and `uv run --frozen pyright` cover `tools/vendor/sync.py` (I ran
  neither).
- Add the blaze-wasm `[patch]` line and regenerate `crates/blaze-wasm/Cargo.lock` and
  `tools/nix/units/daemon.json`.
- Optionally, run `uv run --frozen tools/vendor/sync.py '{"crate": "<c>", "version": "<v>", "check": true}'`
  in CI to keep each vendor tree honest.
