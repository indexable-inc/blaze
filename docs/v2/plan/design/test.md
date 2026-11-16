---
status: plan
summary: What a test is on the rebuilt path. A test unit is a unit like any other with `test` in its cfg and its own key; its tests are one record per unit built from attributes alone (crates/blaze-scope/src/tests_record.rs); the runner is blaze's own, one process per test over one binary; an edit reruns only the tests that reach it, by a tests-by-reach record. Built 2026-11-16: test units become crates of the names set, and the tests record with its reader. Not built: the runner, the reach record, integration-test units, benches, doc tests.
date: 2026-11-16
---
# Tests

The milestone is "a real project's leaf crate passes its own tests, built by blaze", and the
libraries' own suites are the oracle for everything below. rustc's `--test` output and libtest's
tables are references, not the spec (rule 5): nothing here calls `test::test_main_static`.

## Sources, quoted

- Reference, Testing attributes: "The `test` attribute marks a function to be executed as a test.
  These functions are only compiled when in test mode." "The `ignore` attribute tells the test
  harness to not execute that function as a test." "The `should_panic` attribute makes the test
  only pass if it actually panics", with "an input string that must appear within the panic message".
- Reference, Conditional compilation: "`test`: Enabled when compiling the test harness."
- Cargo, Specifying dependencies: dev-dependencies "are not used when compiling a package for
  building, but are used for compiling tests, examples, and benchmarks."

## A test unit

A package's library compiled for tests is a second UNIT (`UnitKind::Test { bin: None }`; a
binary's is `Test { bin: Some(name) }`): the same root file, `test` in its cfg set, the
dev-dependencies among its dependencies. The unit graph makes it only when tests are asked
(`Ask::tests`), and its cfg, kind and binary name are in its key, so no record of it mixes with
the plain library's. What is shared between the two today, and what is not:

- shared: every record keyed by text alone (a file's outline) and every dependency unit whose
  own key is equal (the toolchain's crates, each dependency built once for both);
- NOT shared: anything keyed by an `ItemKey`. An item's key starts with its crate key's prefix
  (`make_key`), and the two units have two crate keys, so an item with no `cfg(test)` in or under
  it has the same text and two keys, and is solved, checked and built twice. The fix belongs to
  the key, not to a special case: a body's records keyed by code identity (code-identity.md), under
  which an instance that reaches nothing the `test` cfg changes is one record for both units.
  Until then a test unit costs a second front-end pass over its crate; say so in any measurement.

## The tests record

One record per test unit, written when its names are solved, from ATTRIBUTES ALONE: no body is
lowered to list tests. One fixed 64-byte row per `#[test]` (or `#[bench]`) function: its item key,
its path as printed (`module::name`, the crate's name left off), ignore and its reason,
should_panic and its expected text; rows sorted by (path, key), strings in the items' key order,
so any worker writes equal bytes. A true `cfg_attr` stands as its attributes
(`#[cfg_attr(miri, ignore)]`); an unread predicate, a `should_panic` form not known, `#[test]` on a
member of an impl: each a typed refusal naming the function. A function an attribute macro
returns (`#[tokio::test]`) is listed under its own key in the call's expansion, where the macro
wrote `#[::core::prelude::v1::test]`. A unit without `test` in its cfg has no tests record
(`NotATestUnit`). The reader checks bounds and finds a test by path with a binary search in place.

## The runner

Each test is a function instance the builder builds like any other; the program is a manifest
over the same code records plus one generated entry, made from the tests record: it reads a row
number from its first argument and calls that row's function. ONE PROCESS PER TEST, started by
`blaze test` (the command, not generated code), each with a deadline:

- the builder has no unwinding, so a panic ends its process; with one process per test that is
  exactly the verdict (returned: status 0; panicked: the abort, its message on stderr), and
  `should_panic(expected = "..")` is judged by the parent reading that message. One process that
  stops at the first panic would report one failure per run and could not judge `should_panic`.
- a hang, a stack overflow or a wild write fails one test with its name (rules 18, 20), tests run
  on every core without sharing state, and a test's result is a record of one process's run,
  which is the shape test-selection.md keys.
- cost: one spawn per test (the floor is the kernel's process start, about a millisecond; tempo's
  4,005 test bodies are seconds across cores), paid only by tests that rerun.

Output: one line per test in path order (`test tests::panics ... ok`, `FAILED`, `ignored, slow`),
the failures' messages, one summary line (`3 tests: 2 passed, 0 failed, 1 ignored`); status 0 when
none failed, else 1. First cut: tests returning `()`; one returning a `Termination` type is refused
by name until the entry calls `report`.

## Incremental: tests by reach

The reach walk (crates/blaze-ladder/src/front2/reach.rs) from a test's function lists every body
it can run. Kept as a record per test (the sorted keys of the bodies reached, valid by the walk's
read list) and, per unit, its inverse: sorted (body key, test key) pairs. An edit to one body
finds its tests by a binary search there; only those are rebuilt (each instance by its own key)
and rerun; every other test's result record still answers. test-selection.md narrows further, to
the functions a test executed.

## The other targets

- Integration tests (`tests/*.rs`): each file its own unit, kind test, depending on the library's
  PLAIN unit and the dev-dependencies; same record, same runner. Not built: blaze-manifest's layout
  does not read `tests/`, `examples/` or `benches/`, and the unit graph has no kind for them.
- `#[bench]`: listed in the tests record with its own flag so nothing is dropped quietly; the
  runner prints it as not run. Measuring is its own design.
- Doc tests: later work, in doctests.md.
