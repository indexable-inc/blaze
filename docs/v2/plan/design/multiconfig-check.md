---
status: plan
summary: "One `blaze check` over every configuration of a package at once (its targets, feature sets and platforms), and the reachability lints that gives: `blaze::dead` (no configuration uses the item; rustc's `dead_code` over all of them), `blaze::test_only` (only the tests do; `--fix` adds `#[cfg(test)]`) and `blaze::cfg_only` (only some features or platforms do; the narrowest exact `#[cfg]`). Slice 1 is built (2026-11-04, lane multiconfig-check: the host's targets under the features asked for, the three lints, the `#[cfg]` fix with a soundness check); multi-platform and feature-set configurations, tagging every diagnostic with its configurations, and dependency findings are designed here and left."
date: 2026-11-04
---
# Checking every configuration at once

Lane multiconfig-check, 2026-11-04. Andrew: "By default deny dead code; ideally a smart
system: if something is only used in tests it must be flagged as that, like `#[cfg(test)]`;
also for methods, structs; or for that target etc. Ideally `blaze check` works cross-target all
at once."

rustc lints one crate as one configuration builds it. A helper only the unit tests call is
"never used" in the lib's check and fine in the lib test's, so `cargo check --all-targets`
warns about it once and says nothing useful: the fix is `#[cfg(test)]`, not deleting it. The
same goes for an item only a feature or only Windows uses. blaze checks every configuration of
the package in one job already (the lib, the lib as a test, each bin, test, bench, example), so
it can say which configurations use each item, and lint that.

## What it says

For every item `dead_code` would report somewhere, the set of configurations that use it:

| configurations that use it | lint | fix |
|---|---|---|
| none | `blaze::dead` ("function `f` is never used") | none (removing code is the author's call) |
| only test builds (each has the `test` atom) | `blaze::test_only` ("only used by tests") | `#[cfg(test)]` |
| some, not only tests | `blaze::cfg_only` ("only used when `feature = \"x\"`") | the narrowest exact `#[cfg]`, when one is |

Each finding names, as data ([`blaze_diagnostics::reach::Finding`]: `used_in`, `unused_in`,
`predicate`), the configurations that use the item and those that have it and don't; the
rendered note says the same in words. When no test configuration was checked, a `blaze::dead`
finding says `blaze check --all-targets` would check the tests too, which may use it.

A finding of an item whose owner has a finding of the same lint about the same configurations
is left out: the methods of an inherent impl of a test-only struct, a trait's items, an enum's
variants, a struct's fields go with it.

### Levels

Each lint warns by default; none is denied by default (`superset-of-rust`: a program that
builds keeps building). `#[allow]`, `#[expect]`, `[lints.blaze]` and `-A`/`-W`/`-D` set them as
any of blaze's lints. The group `blaze::reach` holds the three; `reach = "deny"` under
`[lints.blaze]` (or `[workspace.lints.blaze]`) is the strict mode, and a denied finding fails
the build as a unit's denied lint does. An item `dead_code` is allowed or expected at is left
alone by all three. `blaze::dead` is at the stronger of its own level and `dead_code`'s.

`blaze check` leaves rustc's `dead_code` out of a linted unit's diagnostics: `blaze::dead`
says it, over every configuration, so an item another configuration uses is never called dead.
`dead_code_pub_in_binary`, and every crate that is not linted (a dependency, any command but
`check`), are as before. `#[expect(dead_code)]` is fulfilled as before: the unit's check still
runs the pass.

### The `#[cfg]` predicate

Over the configurations that have the item (one whose `#[cfg]` already leaves it out is not
counted), the narrowest predicate that holds in exactly those that use it, tried in order:

1. one atom, ranked `test`, `feature`, `target_os`, `target_family`/`unix`/`windows`,
   `target_arch`, `target_env`, `target_pointer_width`, then the rest, ties by name; then one
   atom's negation;
2. `any(...)` of values of one key (`any(target_os = "linux", target_os = "macos")`);
3. `all(a, b)` of two atoms (when at most 32 atoms differ);
4. none: the finding says no simple `#[cfg]` names exactly those configurations, and gives no
   fix.

Only atoms some configuration has and another lacks are tried.

### The fix, and when it is sound

The fix inserts `#[cfg(P)]` at the item's start, indented as its line, and at the start of
each impl that goes with it (an impl of the type or of the trait). An impl a derive or an
attribute macro on the type writes goes with the type as if in it, and needs nothing. Only an
item written in a file can take one (not one a macro makes): a free item, an item of an
inherent impl, or an impl. A field or variant gets none.

A fix is machine-applicable (so `--fix` makes it) only when it is sound in every configuration
checked: in each configuration that does not use the item, nothing that stays names it. The
check walks, per configuration, the code that configuration does not use, recording every name
each definition resolves (a pattern's too, which liveness ignores) as (referrer, referred)
pairs; an edge into the item or its impls from a referrer no applied fix leaves out makes the
fix unsound (machine-applicable no longer, with a note naming the referrer and the
configuration). Taking one fix out can make another unsound, so this runs to a fixpoint,
bounded by the number of fixes.

Code no checked configuration compiles can name the item too (`#[cfg(windows)]` code on a
Linux check). Each configuration records the code its `#[cfg]`s leave out, with the names in
it; a piece left out of every configuration that has its file is uncovered, and a fix of an
item whose name is in one is machine-applicable no longer, with a note naming the predicate and
the file. `--fix`'s what-if check is the last net: a fix that breaks a checked configuration
writes nothing.

## How it is built

- **Observation** (`crates/blaze-diagnostics/src/lints/names/dead/observe.rs`): for one unit's
  crate, the dead-code pass as `dead_code` runs it (the same numbering, seeds, liveness and
  report, `report::reported` keeping what it would report with its level instead of building
  diagnostics), and for each definition its key, span, `#[cfg]` insertion point, whether it is
  used, whether it is reported, its four levels, its parent and owner, and its companion impls;
  the reference pairs (`mark::references`, the mark pass with an edge recorder); the cfg atoms;
  the files; the code left out. All plain serializable data.
- **Merge** (`crates/blaze-diagnostics/src/reach.rs`): joins the observations of one package
  by key, the file as diagnostics show it (a test build's crate root, which carries an appended
  harness, is the lib's file), the bytes of the item's name, its kind and name, and its order
  among items of all of those (two items one macro call makes). Then classify, drop owned
  findings, find predicates, make and check fixes. No database: an observation can be kept
  with a unit's record and merged later.
- **Driver** (`crates/blaze-cargo/src/serve/unit/configs.rs`): from `UnitWriter::finished`,
  once every unit of the job ends, the linted units (`plan.clippy`, check or test mode, not a
  build script, not wasm) grouped by package, each observed in the job's front end, merged,
  rendered and given as the first unit's that has the item. A job whose units failed gives none
  and says so in one note. A configuration whose liveness the pass can't know (a `global_asm!`
  naming an item, a `pub macro`) gives none for its package, and a note naming it.

### The roots, ported from rustc's `dead.rs`

What is live starts from rustc's seeds (`create_and_seed_worklist`, `maybe_record_as_seed`,
`has_allow_dead_code_or_lang_attr`), each kept as rustc has it:

- the entry function: the `#[rustc_main]` one, else the `main` the crate root names, of an
  executable without `#![no_main]`;
- in a test build, the root's `main` and every `#[test]` and `#[bench]` function;
- what is public at `Reachable` (effective visibilities), walked after the rest;
- what `#[allow(dead_code)]` or `#[expect(dead_code)]` covers, walked but never made live (so
  an `expect` is fulfilled), and the variants of an enum it covers;
- what the linker or compiler needs: `#[used]`, `#[no_mangle]`, `#[export_name]`, a linkage
  that is not internal, a lang item, `#[panic_handler]`, a foreign function with
  `#[rustc_std_internal_symbol]`;
- `const _`;
- the items of a trait impl, live once their trait's item (or trait, for the impl) and self
  type are (`check_impl_or_impl_item_live`), solved to a fixpoint.

The walk keeps rustc's rules: a variant in a pattern is not constructed; a field written is not
read; the fields of a `repr(C)` or `repr(transparent)` type and the public fields of a public
type are read; a trait's associated types are live with it; an inherent impl's items make its
self type live; `#[derive(Clone)]` and `#[derive(Debug)]` impls read nothing.

### Cost

Per linted unit, one more run of the dead-code pass (numbering, seeds, marking, report) over
the crate, reading what its check inferred, still in the front end (`Goal::Check` lets go of
nothing, `release.rs`), plus one walk of the code that configuration does not use and one scan
of its files' syntax for `#[cfg]`s that are false. The merge is a sort of every keyed item of
the package and linear passes; the fix check is linear in the reference pairs times the nesting
depth per round. Spans: `reachability over configurations` per package and `reachability
observe` per unit. A unit replayed from its record with a cold front end infers its bodies
again for this: slice 4 keeps the observation with the record.

## Slices

1. **Built (2026-11-04).** The host; the configurations the job checks (lib, lib as a test,
   bins, tests, benches, examples; `--all-targets` for all of them) under the features asked
   for; the three lints with their levels and group; the predicate; the `#[cfg]` fix with the
   soundness check; `dead_code` handed over to `blaze::dead` in linted units. Unit tests:
   `crates/blaze-diagnostics/src/reach/tests.rs`.
2. **Platforms.** `[workspace.metadata.blaze] check-targets = ["x86_64-unknown-linux-gnu",
   "aarch64-apple-darwin", "x86_64-pc-windows-msvc"]`: `blaze check` resolves the unit graph
   once per listed target (the host when none) in one job, as several `--target`s would, and
   checks each. The driver already labels a unit by its platform and the merge already joins
   any number of configurations by package; the work is the unit graph and the front end
   holding several targets' crates of one package at once (each its own base-db crate, as the
   lib and lib test are now), and target-specific build scripts and C toolchains the check
   doesn't run (a check needs no linker, but a build script runs for its target).
3. **Feature sets.** `check-features = "default" | "each" | "powerset" | [[...], ...]` per
   package (cargo-hack's names), with a cap on the number of sets. Each set adds the package's
   configurations again; their atoms differ by `feature = "x"`, which the predicate already
   ranks first after `test`.
4. **Observations kept with the record.** Observe in the unit's check phase (the front end is
   warm there) and keep the observation with the unit's record, keyed as the record is; the
   driver merges kept and new ones. A no-op `blaze check` then infers nothing.
5. **Every diagnostic tagged with its configurations.** Today two units that give the same
   diagnostic (a lib and its test) write it twice; `--fix` and `--baselines` dedupe by hand.
   Tag each diagnostic with the configurations it appears in and write it once, through the
   structured-output branch's report crate (`Finding`'s `used_in`/`unused_in` are the model).
6. **Dependencies.** A dependency only the tests use belongs in `[dev-dependencies]`, one only
   a platform uses under `[target.'cfg(...)'.dependencies]`, one no configuration uses is
   unused: the same sets, of extern crates.
7. **`unused_imports` and the rest across configurations.** An import only one configuration
   uses gets the same treatment; any "unused" lint can be joined the same way.
8. **`#[expect(blaze::test_only)]` and friends across configurations**: fulfilled when any
   configuration's merge gives the finding.

## Follow-ups

- **multiconfig-deny-blaze**: turn on `reach = "deny"` in blaze's own `[workspace.lints.blaze]`
  once `blaze check --workspace --all-targets` finishes on blaze (it does not yet: see the lints
  plan's baselines) and its findings are fixed or allowed with a reason. Not counted in this
  lane (it ran nothing); the count is the run's `blaze::dead` + `blaze::test_only` +
  `blaze::cfg_only` lines, and `--new-baseline blaze::dead,blaze::test_only,blaze::cfg_only`
  can hold them at that count meanwhile.
- `--baselines` counts the three as `blaze::` findings of the package of the unit each is given
  as; a baseline made before this counted `dead_code` nowhere, so the first run after it needs
  `--new-baseline` for them.

## Open questions

- A unit replayed with a cold front end infers its bodies again for its observation (slice 4).
  Is that acceptable until then, or should the driver skip replayed units (and give no finding
  for their packages)?
- Should `blaze::test_only` cover a `pub` item of a library only its integration tests use?
  rustc's pass counts every reachable item live, so today it never does; a workspace-wide mode
  could, for packages no one outside the workspace depends on.
- The lib and a bin that `mod`s the same file are two configurations of one item that differ
  in no atom: such an item gets no predicate. Should the bin's own atoms (none exist) be
  invented, or is "no simple `#[cfg]`" right?
- Does `blaze check` take several `--target`s today? If it does, slice 2 is mostly the config
  key.

## Verify

A fixture workspace (`/tmp/reach-fixture`, one package `reach`):

    # src/lib.rs
    fn helper() -> u32 { 1 }          // only the tests: blaze::test_only, fix #[cfg(test)]
    fn unused() {}                    // nothing: blaze::dead
    pub fn api() -> u32 { 2 }
    #[cfg(test)]
    mod tests { #[test] fn t() { assert_eq!(super::helper(), 1); } }

    blaze check --all-targets                 # a test_only warning on helper, dead on unused
    blaze check                               # helper is dead, with the --all-targets help
    blaze check --all-targets --fix           # writes #[cfg(test)] above helper
    blaze check --all-targets                 # only unused is left

On blaze itself, once it checks its own workspace:

    blaze check --workspace --all-targets
    blaze test -p blaze-diagnostics reach
    blaze test -p blaze-lint-defs
