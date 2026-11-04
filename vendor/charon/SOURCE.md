# charon

- Upstream: https://github.com/AeneasVerif/charon, commit
  `a5443ed4e648b153ea0f052e9c22a3667ef1c0b9` (`charon/`), version 0.1.309.
- Licence: Apache-2.0 (LICENSE.md), copyright the Charon and hax authors.
- Kept: the library `charon_lib` (`charon/src` without `bin/`): the ULLBC and LLBC ASTs
  (`ast`), the transformation passes that turn ULLBC into the LLBC Aeneas reads (`transform`:
  control-flow reconstruction, the resugaring of rustc's lowering, the cleanups), the `.llbc`
  writer and reader (`export`), the pretty printer, the name matcher, options and ids.
- Left out: `charon-driver` and its rustc translation (`src/bin/charon-driver`;
  `crates/hir-ty-charon` replaces it, reading blaze's MIR), the `charon` cargo wrapper
  (`src/bin/charon`), `generate-asts`, the MiniRust output (`src/minirust*`, `minirust-rs`,
  `miniutil`), `rustc_const_uneval`, `rustc_trait_elaboration`, `adt-into`, the build script,
  the tests, examples, `charon-ml` and the rest of the repository.

The crate is outside blaze's workspace (the root `Cargo.toml` excludes `vendor/charon/charon`)
and `crates/hir-ty-charon` depends on it by path.

## What blaze changed

`blaze.diff` is the whole diff against upstream for the kept sources and the manifest. In short:

- Manifest: library only (`charon_lib`), no binaries, tests or build script, `publish = false`,
  the workspace's versions spelled out, and the dependencies nothing kept uses dropped
  (`hax-adt-into`, `rustc_const_uneval`, `rustc_trait_elaboration`, `minirust-rs`, `miniutil`,
  `crates_io_api`, `reqwest`, `flate2`, `tar`, `tracing-tree`, `assert_cmd`, `wait-timeout`,
  `tempfile`, `rustc_version`, `toml`, `which`, `convert_case`, `lazy_static`, `rustc_apfloat`,
  `extension-traits`, `syn`, `log`). The features `charon_on_charon` and `minirust` stay
  declared, never on, so the inert attributes that name them refer to known features.
- `lib.rs`: the `charon_on_charon` block that pulled in `rustc_private` and `rustc_hir`, and the
  `minirust` module, are gone; `COMMIT_ID` is the commit above as a constant (upstream's build
  script ran `git` to set `CHARON_GIT_COMMIT`).
- `export.rs`: the MiniRust serializer arm is gone; asking for MiniRust output is the error
  upstream gives when built without it.
- `logger.rs`: `tracing_tree::HierarchicalLayer` (from a git fork) is `tracing_subscriber`'s
  fmt layer, writing to stderr.

## Updating

Clone upstream at the new commit, copy `charon/src` without `bin/` and `minirust*`, apply
`blaze.diff`, check the manifest's dependencies against upstream's, and bump the commit here
and in `lib.rs`. Aeneas checks the `.llbc` file's `charon_version` for equality, so the Aeneas
used to read blaze's output must be the one built against this Charon version.
