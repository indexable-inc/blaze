# Requests of `diagnostics-items-attrs-stability` (child of `diagnostics-items-attrs`)

## 1. `check.rs`: let the new `Parsed` variants through (parent's file)

`crates/blaze-diagnostics/src/checks/attrs/parsed.rs` has five new variants, appended:
`Stability`, `ConstStability`, `BodyStability`, `RustcConstStableIndirect` and `Deprecated`.
`check::one` in `crates/blaze-diagnostics/src/checks/attrs/check.rs` matches `Parsed`
exhaustively, so the crate does not build until they join its no-op arm:

```rust
        | Parsed::MacroExport
        | Parsed::Stability(_)
        | Parsed::ConstStability(_)
        | Parsed::BodyStability(_)
        | Parsed::RustcConstStableIndirect
        | Parsed::Deprecated(_) => {}
```

Why: `check_mod_attrs` checks none of them. Their checks are rustc_passes' stability pass,
which `parsers::stability::crate_checks` does (request 2).

> forwarded to diagnostics: between its own children.

> applied (diagnostics-items-attrs): the five variants are in `check::one`'s no-op arm.

## 2. `checks/attrs.rs`: call `parsers::stability::crate_checks` (parent's file)

E0549, `UselessStability`, `CannotStabilizeDeprecated`, `MissingConstErr`,
`ConstStableNotStable` and `RustcConstStableIndirectPairing` are rustc_passes'
`check_compatible_stability`. Each one reads the stability its definition inherits from its
parents, which a per-node `check` can't see. In `checked`, keep what each owner parses to,
with nothing for a macro call, and after the loop push what the pass gives:

```rust
pub(crate) fn checked(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner]) -> Vec<Found> {
    let session = Session::of(cx, owners);
    let mut found = Vec::new();
    let mut all = Vec::with_capacity(owners.len());
    for owner in owners {
        if owner.target == Target::MacroCall {
            all.push(Vec::new());
            continue;
        }
        let parsed = lower(cx, &session, owner, false, &mut Vec::new());
        let mut errors = Vec::new();
        check::check(cx, &session, owner, &parsed, &mut errors);
        push(cx, owner, Phase::Items, errors, &mut found);
        all.push(parsed);
    }
    for (at, diag) in parsers::stability::crate_checks(cx, &session, owners, &all) {
        push(cx, &owners[at], Phase::Items, vec![diag], &mut found);
    }
    found
}
```

The pass gives nothing unless the crate enables `staged_api`. It returns its errors in
rustc's order: module by module, and within a module the items first, then trait items,
impl items and foreign items. In `stability-attribute-sanity.rs` they come after every
parse error and before E0711 from `checks/features/library.rs`. The test
`tests/items/attrs/stability.rs::stability_attributes_are_checked_as_parsed_and_against_each_other`
checks that ordering.

> forwarded to diagnostics: between its own children.

> applied (diagnostics-items-attrs): `checked` calls `parsers::stability::crate_checks` as written here.

## 3. Confirm `RustcVersion::CURRENT` of the pinned nightly (coordinator)

`CURRENT` in `parsers/stability.rs` is `(1, 101, 0)` for `nightly-2026-10-27`. I worked it out
from the six-week release cadence, starting from the rustc checkout's 1.99.0, not from the
toolchain's manifest. It is what `since = "CURRENT_RUSTC_VERSION"` stands for in
`#[deprecated]`, and so in the E0549 and `CannotStabilizeDeprecated` checks. Please check it
against `src/version` of the pinned toolchain and say if it is off.

> applied: confirmed `(1, 101, 0)` for `nightly-2026-10-27` (`harness/toolchain`): 1.99.0 went stable on 2026-10-01 and 1.100.0 is due 2026-11-12, so between them beta is 1.100 and nightly 1.101.0, which `CURRENT_RUSTC_VERSION` expands to. Recheck against the toolchain's `src/version` at the first build.

## 4. Missing stability annotations need effective visibilities (diagnostics)

rustc's `MissingStabilityAnnotations` also gives "{descr} has missing stability attribute"
(`MissingStabilityAttr`) and "{descr} has missing const stability attribute"
(`MissingConstStabAttr`). Both apply only to definitions that are reachable from outside the
crate (`effective_visibilities`), which this lane has no query for. Until one exists they
aren't given. `tests/ui/stability-attribute/missing-stability-attr-at-top-level.rs` and
`missing-const-stability.rs` need them.

> forwarded to diagnostics: between its own children.

> open: with diagnostics, for an effective-visibility query; `MissingStabilityAttr` and `MissingConstStabAttr` are not given until then.
