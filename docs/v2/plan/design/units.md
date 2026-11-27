---
status: current
summary: The unit graph (crates/blaze-unit-graph): which crates a request compiles and with which features and cfgs, by cargo's documented rules, without cargo; a sans-IO function from the lock, the manifests, two platform descriptions and the ask to units, each crate once per configuration; its record; what the driver's Units task calls; what is not covered.
---
# The unit graph

A **unit** is one crate compiled once: a package's library, proc macro, binary, build script or
test harness, for the target or the host, with one feature set. The rules that pick the units
and their features are written once, as the specification, in the doc of
`crates/blaze-unit-graph/src/lib.rs`, each with its Cargo Book source. They cover:

- feature values: `f`, `dep:x`, `x/f` (also turns on the feature `x` when there is one), and
  `x?/f` (waits for `x`, never turns it on);
- implicit features of optional dependencies, which every edition makes;
- `default` and `default-features = false`;
- `--features`, `--all-features` and `--no-default-features`;
- unification, and resolver 1, 2 and 3: the host namespace for build-dependencies and proc
  macros, platform filtering, dev-dependencies only with tests, and the default resolver by
  edition;
- `[target.'cfg(..)']` tables;
- dependency kinds;
- renames (the extern name);
- `[patch]` and `[replace]`, as the lock already holds them;
- inheritance from the workspace, done by blaze-manifest's layout;
- build scripts and `links`;
- the cfg set of each unit.

## The function

`Resolution` is a sans-IO machine:

- `new(&Lock, &target, &host, &Ask, Resolver, alloc)` sets up the request;
- `member(id, Package)` hands in the workspace members;
- `pass()` gives `Pass::Need(Vec<LockId>)` (the locked packages whose manifests it must read
  next), `Pass::Done(UnitGraph)`, or `Pass::Partial(PartialGraph)` when a package the graph
  reaches could not be read;
- `package(id, Option<Package>)` answers a need. `None` means the package's sources could not
  be read (the caller has a `SourceError` naming it, which the driver keeps as a finding). The
  resolution then ends in `Pass::Partial`, which names every missing package and offers no
  `units()`: its consumer fetches them or refuses naming them, and no unit is ever resolved
  with a dependency quietly absent (states.md item 35).

## Where manifests come from

`sources::StoreSources` answers each need from the store's record of the package's sources
(blaze-fetch's pack, found by its `SourceId`), read in place. The store is a passed-in
`Records` and the reader a passed-in `Open`: `PackView::open_source_put` for records this
machine fetched, `open_source` for others (rule 75). Each record is mapped and opened once.

- A crates.io crate is read with blaze-manifest's `Layout` started at the record's root.
- A git package is found as cargo finds it: first among the members of the layout at the
  commit's root, kept for the commit's next packages, else by scanning every `Cargo.toml` of
  the commit for its `package.name` and version. Two directories holding it is
  `TwoInCommit`.
- A missing record is `NotFetched`, naming the package and the key. A source the lock cannot
  verify is `NotFetchable`. A path package that is not a member is `PathOutsideWorkspace`.
  Nothing is read from cargo's caches.

Members are read from the checkout by the caller and handed in with `member`.

## The record

`record::encode(graph, target, host)` writes the graph's bytes (magic `BUG4`): the resolver,
both platforms once, then the units in order. A unit's cfg is not stored: `decode` remakes it
with `unit_cfg`, the one function that makes it, and `encode` refuses a unit whose cfg is
anything else (`NotItsCfg`). The bytes are a function of the graph alone.

`record::Source` is the record's source digest. It covers:

- the lock's bytes, which pin every dependency's sources;
- the workspace manifests and listings read, or the `Units` task's digest of them;
- the two platforms;
- the ask;
- the toolchain's id, when it does not already come through the `Units` digest.

An edit to a member's manifest that changes no resolution changes the source but leaves the
bytes equal (the test `an_edit_that_changes_no_resolution_leaves_the_record_equal`). The
driver's task is `crates/blaze-drive/src/unit_graph_task.rs`, run by blaze-cli's `engine_units.rs`.

The lock is blaze-manifest's `lock` module, the one reader of `Cargo.lock`. A `Package` is
`Package::from_member` of blaze-manifest's `layout::Member`. The targets are `TargetDesc` values
(a triple and its sorted cfg atoms) that the caller passes. Nothing asks a compiler or the
machine.

Inside, there are two explicit worklists and no recursion:

1. The fixpoint over (package, side) nodes. Features, turned-on optional entries and deferred
   weak values only grow, so the result does not depend on the order the work is taken in.
2. A walk from the roots that makes the units.

Each pass re-runs both over the facts known so far. On tempo (aarch64-apple-darwin, all members,
no tests, every dependency read from the fetch store; measured 2026-11-16): 1,066 packages,
1,263 units (324 for the host), 9 passes, 79 ms in all (51 ms of it dependency manifests), a peak
footprint of 24 to 35 MiB and a record of 258,968 bytes.

## The output

`UnitGraph { units, resolver, meta, left_out }`. Each `Unit` has:

- `package` (a `LockId`), `name` and `version`;
- `kind`: `Lib`, `ProcMacro`, `Bin(name)`, `BuildScript`, `Test{bin}`, `IntegrationTest`,
  `Example` or `Bench`;
- `for_host` and `edition` (a `PackageEdition`, so blaze editions are kept);
- `crate_name` and `root` (the root file, relative to the package's directory);
- `features`, sorted;
- `cfg`, sorted: the platform's atoms, `feature="f"` for each feature, `debug_assertions`, `test`
  on a test unit and `proc_macro` on a proc macro;
- `deps` as (extern name, `UnitId`), sorted;
- `build_script: Option<UnitId>`: that script's `rustc-cfg` outputs join the unit later;
- `links`.

Units are sorted by package (name, version, source), then kind, then configuration, and ids are
positions in that order. Two runs therefore write equal bytes (rule 25).

## What the driver's Units task calls

The Units task (blaze-cli's `units.rs` and `engine_units.rs`) runs blaze-manifest's `Layout`,
then `Resolution`, answering each `Need(LockId)` with the package's manifest:

- a registry crate's from its unpacked source;
- a git package's from its checkout at the locked commit, through a `Layout` anchored at the
  checkout's root, so that inheritance works.

The record is keyed by the lock's bytes, the manifests read, the ask and the target
description. The Units task's record becomes the unit graph's bytes, and blaze-scope's
`GraphCrate` is one unit.

The sysroot (core, alloc, std) is the same function run as its own request on `library/`'s lock
and manifests.

## Not covered

- profiles beyond check's dev profile;
- artifact dependencies;
- registries that need a login;
- `-Z` features;
- a build script's outputs, which join after it runs.
