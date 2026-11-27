---
status: plan
summary: "Doctests built and run by blaze with its own front end, never rustdoc: rustdoc's rules for finding examples, writing their programs (line-aligned to the docs) and judging them, ported; each example is its own unit (its own crate, binary and process, kept and rebuilt per example like any blaze test), never merged, in any edition. Not built on the item layer: blaze test runs no doctest today."
date: 2026-11-02
---
# Doctests without rustdoc

blaze never runs rustdoc (`no-rustc`), so it finds a library's examples, writes their programs
and builds and runs them itself, with the rules rustdoc has at the pinned nightly
(`superset-of-rust`: a doctest that passes, fails or is ignored under `cargo test --doc` does the
same under `blaze test`). Doctests run by default under `blaze test` (D81). None of this is built
on the item layer: `blaze test` finds and runs no doctest today.

## Prior art

- rustdoc (`src/librustdoc/doctest.rs`, `doctest/make.rs`, `doctest/runner.rs`,
  `doctest/rust.rs`, `doctest/markdown.rs`, `html/markdown.rs`): the reference. It collects
  examples by walking the crate's HIR with its attributes (`HirCollector`), parses each code
  block's info string (`LangString`), writes a program per example (`DocTestBuilder`,
  `generate_unique_doctest`) and since edition 2024 merges the mergeable ones into one crate
  per edition (`DocTestRunner`, `run_merged_tests`), each still run in a child process of the
  merged binary.
- cargo: plans one `Doctest` unit per library, after the library and its dev-dependencies, and
  hands it to rustdoc `--test`; `cargo test --doc` runs only those.
- cargo-nextest: no doctests (nextest-rs/nextest#16): it runs each test in its own process but
  leaves doctests to `cargo test --doc`, since they are not in a test binary it can list.
- Prior rustdoc-free runners (skeptic, doc-comment, `doctest-file`) generate tests from
  markdown with a build script and rustc; none has rustdoc's rules.

## Decision: every example is its own unit, never merged

rustdoc merges an edition-2024 library's examples into one crate per edition to save compile
time. blaze does not, in any edition: each example is a unit of its own, a crate with one
binary, run in a process of its own. This is a deliberate difference from rustdoc
(`useful-not-parity`), for three reasons:

1. **Isolation.** An example runs alone, as nextest runs a test: its globals, environment,
   panics, aborts, signals and leaked threads touch no other example, and its result is its
   own. rustdoc's merged binary re-runs itself per example for most of this, but examples
   still share one crate's statics, link-time items and `#![...]` attributes (rustdoc falls
   back to standalone when an example's attributes can't be merged, which shows that merging
   changes meaning).
2. **Per-example reuse.** With one unit per example, an edit to one example rebuilds that
   example alone; every other is found unchanged by its key and is not rebuilt or re-run. A
   merged crate makes every edit to any example an edit to the one crate everyone shares.
3. **One model for `blaze test`.** Unit tests, integration tests and doctests are all units the
   command builds, schedules, keeps and runs the same way, with the same output,
   `--keep-going`, filters, sandboxing and profile spans. The compile time merging saves comes
   from blaze's own reuse instead: an example's crate depends on the already-built library,
   and its program is a few functions.

What changes from `cargo test --doc` because of it: an example that only passes merged (it
reads a static another example set, or relies on the merged crate's attributes) or only
passes alone. None has been found yet; the outcome comparison below lists any it finds.

## The rules, ported from rustdoc

From rust-lang/rust master of 2026-11-02 (the pinned nightly's line; `pins/toolchain`); they
belong in the front end (`semantics-in-front-end`).

| rule | rustdoc |
|---|---|
| which items are walked: the crate, modules, functions, ADTs with variants and fields, consts, statics, type aliases, traits and their items, impls and their items, macros, `use` and `extern crate` items, extern blocks | `doctest/rust.rs` `HirCollector` |
| test names: `{file} - {item path} (line {n})`, the file relative to the package, the path with no spaces | `doctest.rs` `ScrapedDocTest::new` |
| the line: the code block's opening fence, in the file the doc is written in | `doctest/rust.rs` `get_doctest_line` |
| code blocks: fenced and indented, Rust unless the info string names another language | `html/markdown.rs` `find_codes` |
| attributes: `ignore`, `ignore-<target>`, `no_run`, `should_panic`, `compile_fail`, error codes `E0xxx`, `edition20xx`, `test_harness`, `standalone_crate`, `rust`, unknown words (the tag iterator: `{.class}` and `{class=..}` blocks, `(comments)`, quoted strings, separators ` `, `,` and tab; an `ignore-x` turns a plain `ignore` off; `E` and four digits only; an unknown edition is none) | `html/markdown.rs` `LangString::parse`, `TagIterator` |
| hidden lines: `# x` is code, `##` is `#`, a lone `#` is an empty line | `html/markdown.rs` `map_line` |
| `#![doc(test(attr(..)))]` and `#![doc(test(no_crate_inject))]` | `doctest.rs` `scrape_test_config` |
| the example's inner attributes (lints to the module, the rest to the crate), its `extern crate`s and the text between, `fn main` found among items or written by a macro (a macro call statement only in braces or with its `;`, rustc's `StmtKind::MacCall`; one in parentheses with no `;` is an expression), a lone `;` kept with the rest, anything but items means no main of its own, a parse error compiles it as written | `doctest/make.rs` `parse_source`, `check_item`, `DocTestBuilder` |
| the program: `#![allow(unused)]` or the crate's test attributes, the example's crate attributes, module attributes and crates, an injected `extern crate r#lib` when the text names the library and has none, a `fn main` wrapping `_doctest_main_{id}` (returning a `Result` when the example ends with `(())`) unless the example has one, uses `test_harness` or is `no_std` | `doctest/make.rs` `generate_unique_doctest` |
| the program's id: the file with every non-alphanumeric as `_`, the line, the example's number at that line | `doctest.rs` `generate_test_id` |
| edition: the example's `edition20xx`, else the library's | `doctest.rs` `run_test` |
| ignore for a target: `ignore-x` matches when the target triple contains `x` | `doctest/runner.rs` `add_test` |
| line offset: the lines the program says it adds before the example's (its `extern crate` counted as one line though it writes two), which rustc is told to move the example's lines by | `doctest/make.rs` `generate_unique_doctest`, `doctest.rs` `UNSTABLE_RUSTDOC_TEST_LINE` |
| verdicts: `compile_fail` passes when refused with each error code (`error[E..]` in the errors), any other fails when refused ("Couldn't compile the test."), `no_run` passes once built, `should_panic` passes on any failing exit, any other on success; rustdoc's words for each | `doctest.rs` `run_test`, `TestFailure` |

## How it runs (the design)

1. **Discovery.** A library's examples are read from its doc comments once its names and
   expansions are done (`#[doc = ..]` attributes included), per module as tests are (bottom.md
   section 11); the list is a record keyed by the library's docs and its test attributes, so
   adding, removing or editing an example adds, removes or rebuilds that example's unit only.
2. **Each example's crate.** A crate whose root is the example's written program, with the
   edition of the program, the library's cfgs (its features) but neither `doc` nor `doctest`
   (rustdoc sets those only to collect), and the library and its dev-dependencies as
   dependencies. A `test_harness` example is built as rustdoc's `--test` builds it: a test crate
   with `cfg(test)` and libtest's harness, so its `#[test]`s run under its own harness and it
   needs no `main`. Its key covers the program's source, the library and its flags
   (`keys-cover-every-input`); an edit of one example's doc text leaves every other example's
   key as it was.
3. **Build and run.** The crate is built as a test program is and run in its own process by the
   test runner, sandboxed like tests. The runner reports it under the libtest name rustdoc gives
   it, and judges it as rustdoc does: `ignore` is reported ignored and not built, `no_run` is
   built and not run, `should_panic` passes only on a panic exit, `compile_fail` passes only
   when the build fails, and with error codes only when each code is among the errors.
4. **Lines.** A panic or diagnostic in the program is reported at the doc's line, in the
   documented file, as rustdoc has rustc report it (`UNSTABLE_RUSTDOC_TEST_LINE` and
   `UNSTABLE_RUSTDOC_TEST_PATH`), with no remapping: the written program puts each line of the
   example's rest on the line rustdoc shows it at (what rustdoc adds is joined onto as few lines
   as it fits, then padded with newlines). One known difference: `line!()` in an example gives
   the shown line, where rustdoc's gives the program's own.

A unit blaze can't build fails alone: by default every other unit builds and its tests run,
and the run fails, naming the unit and the tests it held back; under `--fail-fast` the run
stops, naming the unit.

## Open

1. Extraction details: items under `#[cfg(doctest)]` (rustdoc builds the library with
   `cfg(doctest)` when collecting), docs from `#[doc = include_str!(..)]` (the line is the fence
   line in the included file), `#[doc(hidden)]` items (rustdoc still tests them), re-exports
   (rustdoc tests an item where it is defined, once), `#[doc(test(attr))]` on modules and items
   read down each example's path, and items whose `#[target_feature]`s the target lacks
   (skipped with what is inside them).
2. The outcome comparison on crates with many doctests (`smallvec`, `itertools`,
   `regex-syntax`), each example's outcome against `cargo test --doc` at the pinned nightly.
