# Requests from fix-names-features

## 1. Workspace dependency `blaze-phf` (root `Cargo.toml`), needed by `hir-ty`
- What: the `blaze-phf` workspace dependency fix-names requested (`v2/requests/fix-names.md`,
  item 1). `crates/hir-ty/Cargo.toml` now has `blaze-phf.workspace = true`.
- Why: `hir-ty/src/target_feature.rs` builds its name table with `blaze_phf::Phf`.

> applied: already in, from fix-names 1.

## 2. (design, for fix-names or the coordinator) Move the target feature table below `cfg`
- What: move `TargetFeature`, `TargetFeatureSet`, `NAMES`, `BY_NAME`, `IMPLIED` and rustc's
  per-arch tables out of `crates/hir-ty/src/target_feature.rs` into a crate below `cfg` (say
  `crates/target-features`, depending only on `blaze-phf`), re-exported from `hir-ty`. That part
  of the file uses nothing of `hir-ty`; only `TargetFeatures`, `Declared` and the two tracked
  queries need the database.
- Why: two remaining per-check sets of feature names live below `hir-ty` and cannot name the
  enum today:
  - `crates/cfg` (`CfgOptions { enabled: FxHashSet<CfgAtom> }`): every `cfg(target_feature =
    "…")` is an `FxHashSet<CfgAtom>` probe; with the enum it can keep the crate's target
    features as a `TargetFeatureSet` and answer those atoms by a bit test (and
    `hir_ty::crate_target_features` becomes a field read).
  - `crates/hir-def/src/attrs.rs` (`AttrFlags::target_features -> &FxHashSet<Symbol>`): could
    return the bitset plus unknown names directly, and `hir_ty::target_feature`'s
    `declared_target_features` query would go away.
- Owners: `cfg` is not in `crates.md`'s lane table; `hir-def/src/attrs.rs` is db's.

> forwarded to coordinator: a design choice across lanes (fix-names is told too).

## 3. (for fix-intrinsics / codegen) `asm::extensions` by feature, not by name
- What: `crates/hir-ty-codegen/asm.rs`'s `extensions(arch, impl IntoIterator<Item = &str>)`
  maps each name by `aarch64_version` / `aarch64_extension` string matches. It could take a
  `hir_ty::TargetFeatureSet`, with a `const` table `[Option<&str>; TargetFeature::COUNT]`
  (and one of versions) built by `TargetFeature::named`. Its callers (`mod.rs` inline `asm!`,
  `v2/refs.rs` inline `asm!`, and the NEON lowering through `neon::Lowered::features`) would
  pass `TargetFeatures::from_fn_no_implications(..).known()` or a set.
- Why: per `asm!` block and per NEON intrinsic, cold next to the assembly text it builds, so I
  left it: `asm.rs` is fix-intrinsics' file right now.

> forwarded to codegen (and fix-intrinsics).
