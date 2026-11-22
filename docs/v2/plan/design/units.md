---
status: plan
summary: The unit graph (crates/blaze-unit-graph): which crates a request compiles and with which features and cfgs, by cargo's documented rules, without cargo; a sans-IO function from the lock, the manifests, two platform descriptions and the ask to units, each crate once per configuration; what the driver's Units task calls; what features as conditions change; what is not covered.
date: 2026-11-16
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
  with a dependency quietly absent (states.md item 35; before 2026-11-20 the graph answered
  `Done` with `unavailable` and `incomplete` fields nothing read).

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

`record::encode(graph, target, host)` writes the graph's bytes (magic `BUG1`): the resolver,
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
driver's task is `blaze-drive/src/unit_graph_task.rs`, unwired until the driver lane picks
its `Kind` and `Name`.

The lock is blaze-manifest's `lock` module, the one reader of `Cargo.lock`. A `Package` is
`Package::from_member` of blaze-manifest's `layout::Member`. The targets are `TargetDesc` values
(a triple and its sorted cfg atoms) that the caller passes. Nothing asks a compiler or the
machine.

Inside, there are two explicit worklists and no recursion:

1. The fixpoint over (package, side) nodes. Features, turned-on optional entries and deferred
   weak values only grow, so the result does not depend on the order the work is taken in.
2. A walk from the roots that makes the units.

Each pass re-runs both over the facts known so far. On tempo (aarch64-apple-darwin, all
members, no tests, 2026-11-16, every dependency read from the fetch store):

- 1,066 packages (118 git, 52 path, 896 registry) and 1,263 units (324 for the host);
- 79 proc-macro units and 106 build-script units;
- 0 packages unavailable (it was 143, which hid 711 packages);
- 9 passes;
- 79 ms in all:
  - lock 3.7 ms;
  - workspace layout 7.0 ms;
  - dependency manifests 51 ms;
  - resolution 16.5 ms;
- a peak footprint of 24 to 35 MiB;
- a record of 258,968 bytes.

## The output

`UnitGraph { units, unavailable, resolver }`. Each `Unit` has:

- `package` (a `LockId`), `name` and `version`;
- `kind`: `Lib`, `ProcMacro`, `Bin(name)`, `BuildScript` or `Test{bin}`;
- `for_host` and `edition` (a `PackageEdition`, so blaze editions are kept);
- `crate_name` and `root` (the root file, relative to the package's directory);
- `features`, sorted;
- `cfg`, sorted: the platform's atoms, `feature="f"` for each feature, `debug_assertions`, `test`
  on a test unit and `proc_macro` on a proc macro;
- `deps` as (extern name, `UnitId`), sorted;
- `build_script: Option<UnitId>`: that script's `rustc-cfg` outputs join the unit later;
- `incomplete`.

Units are sorted by package (name, version, source), then kind, then configuration, and ids are
positions in that order. Two runs therefore write equal bytes (rule 25).

## What the driver's Units task calls

The driver lane owns `units.rs` in blaze-drive. Its Units task runs blaze-manifest's `Layout`,
then `Resolution`, answering each `Need(LockId)` with the package's manifest:

- a registry crate's from its unpacked source;
- a git package's from its checkout at the locked commit, through a `Layout` anchored at the
  checkout's root, so that inheritance works.

The record is keyed by the lock's bytes, the manifests read, the ask and the target
description. The Units task's record becomes the unit graph's bytes, and blaze-scope's
`GraphCrate` is one unit.

The sysroot (core, alloc, std) is the same function run as its own request on `library/`'s lock
and manifests.

## Features as conditions (not built)

Today a unit holds one configuration: `features`, a sorted list, and `cfg`, the atoms that list
and its platform make. What the predecessor left for conditions: the output keeps features
apart from the rest (a list per unit, never folded into ids or the sort key beyond the
configuration index), cfg is made by one function (`resolve::unit_cfg`) and the record does not
store it, so swapping the list for a condition touches one field, one function and one record
section. What changes, exactly:

1. **Variables.** Each feature of each package side is a blaze-cond variable
   `feature:<package>@<version>/<side>/<feature>` (conditions.md's `feature:` with the version
   and side added, since two versions or sides of one package are separate namespaces). An
   optional dependency `x` is the variable of its `dep:x` feature.
2. **Rules 1 to 5 become clauses, not a fixpoint over bits.** `f = ["g"]`: `f -> g`;
   `"dep:x"`: `f -> x_on`; `"x/g"`: `f -> x_on & x.g` (and `f -> x` where the feature `x`
   exists); `"x?/g"`: `f & x_on -> x.g`; an entry with defaults on: `edge_on -> x.default`; an
   entry's `features = [..]`: `edge_on -> x.each`. Unification is a disjunction: a feature of a
   package is on iff some clause forces it. The fixpoint stays as the order the clauses are made
   in; its state per node becomes a `Cond` per feature instead of a bit.
3. **The ask.** `--features` and the defaults stay constants in a single-configuration request.
   An all-configurations request leaves each root's features free (their variables are the
   request's choices) and conjoins the clauses into the request's `valid` (conditions.md).
4. **`Unit::features`** becomes `AVec<(Text, Cond)>`: every feature the package defines, with
   the condition it is on under. A unit whose features are all `Cond(0)`/`Cond(1)` is today's
   unit. **`Unit::cfg`** stays the platform's atoms and flags only; `feature = "f"` atoms are
   no longer listed, since the variable is the atom.
5. **Edges.** An optional dependency's edge (`UnitDep`) gains the `Cond` of `x_on`; a
   platform-specific edge stays decided per platform. A unit splits into two only where a
   feature changes which unit an edge points at (another configuration of the dependency);
   elsewhere one unit carries every configuration.
6. **The record.** Each unit's features become `(name, condition rows)` in blaze-cond's
   portable form, rows in one canonical order (rule 25), and `UnitDep` gains a condition.
   The key's `Source` gains a byte saying whether the request has one configuration or all.
   The early-cutoff test stays: an edit that changes no clause leaves the bytes equal.
7. **Not changed:** the walk from the roots, the sources reader, the lock, the sort order of
   units, and `build_script` (a script runs per configuration it is asked for, so the
   build-script task forks on the features it reads, not the graph).

The size bound to check first: conditions.md's `sat_count` stops at 127 variables, and tempo's
graph has more features than that across packages. Only the roots' free variables count
toward it, since the others are forced by clauses.

## Not covered

- profiles beyond check's dev profile;
- artifact dependencies;
- registries that need a login;
- `-Z` features;
- examples and benches as units;
- a build script's outputs, which join after it runs.
