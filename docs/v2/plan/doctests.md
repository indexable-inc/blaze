---
status: plan
summary: "Doctests built and run by blaze with its own front end, never rustdoc: rustdoc's rules for finding examples, writing their programs (line-aligned to the docs) and judging them, ported; each example is its own unit (its own crate, binary and process, patched on an edit like any blaze test), never merged, in any edition; what is built (2026-11-02, lanes doctest to doctest5: `blaze test --doc` builds each example as its own crate in parallel, builds and links only an edited one while every other is found unchanged by one store lookup, builds `test_harness` examples as tests, and runs each in its own process) and what is left."
date: 2026-11-02
---
# Doctests without rustdoc

Lane doctest, 2026-11-02. blaze never runs rustdoc (`no-rustc`), so it finds a library's
examples, writes their programs and builds and runs them itself, with the rules rustdoc has at
the pinned nightly (`superset-of-rust`: a doctest that passes, fails or is ignored under
`cargo test --doc` does the same under `blaze test`).

## What happened before this

- v2 refused every `blaze test` up front: blaze-units planned a `Doctest` unit for each
  library (as cargo does under `test`) and `CompileMode::of` had no plan for it, so the whole
  request stopped with "blaze does not run rustdoc: the `Doctest` unit of … has no plan"
  before any unit was built.
- v1 ran doctests itself, with no rustdoc (`crates/blaze-engine/src/doctest*.rs`, deleted on
  2026-10-30, last at `5d69a67f48^`): it found examples with rust-analyzer's attrs, wrote
  rustdoc's program for each, compiled all of a library's examples as one crate of modules and
  ran them under one libtest harness. Its program writer and its extraction are the starting
  point of `blaze-doc`'s; the merged crate is not kept (below).

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
2. **Per-example patching.** blaze patches a built test in place when its source changes
   (live patching) instead of rebuilding it. With one unit per example, an edit to one example
   patches that example's binary alone; nothing else is rebuilt, linked or re-run. A merged
   crate makes every edit to any example an edit to the one crate everyone shares.
3. **One model for `blaze test`.** Unit tests, integration tests and doctests are all units the
   daemon builds, schedules, caches, patches and runs the same way, with the same output,
   `--keep-going`, filters, sandboxing and profile spans. The compile time merging saves comes
   from blaze's own reuse instead: an example's crate depends on the already-built library,
   and its program is a few functions.

What changes from `cargo test --doc` because of it: an example that only passes merged (it
reads a static another example set, or relies on the merged crate's attributes) or only
passes alone. None has been found yet; the outcome comparison below lists any it finds.

## The rules, ported from rustdoc

All in `crates/blaze-doc` (front end crate; `semantics-in-front-end`), from rust-lang/rust
master of 2026-11-02 (the pinned nightly's line; `harness/toolchain`).

| rule | rustdoc | blaze |
|---|---|---|
| which items are walked: the crate, modules, functions, ADTs with variants and fields, consts, statics, type aliases, traits and their items, impls and their items, macros, `use` and `extern crate` items, extern blocks | `doctest/rust.rs` `HirCollector` | `doctest.rs` `examples`, `Found` |
| test names: `{file} - {item path} (line {n})`, the file relative to the package, the path with no spaces | `doctest.rs` `ScrapedDocTest::new` | `doctest.rs` `test_name` |
| the line: the code block's opening fence, in the file the doc is written in | `doctest/rust.rs` `get_doctest_line` | `doctest.rs` `place` |
| code blocks: fenced and indented, Rust unless the info string names another language | `html/markdown.rs` `find_codes` | `doctest.rs` `blocks` |
| attributes: `ignore`, `ignore-<target>`, `no_run`, `should_panic`, `compile_fail`, error codes `E0xxx`, `edition20xx`, `test_harness`, `standalone_crate`, `rust`, unknown words | `html/markdown.rs` `LangString::parse`, `TagIterator` | `markdown.rs` `LangString`, `tokens` (the tag iterator: `{.class}` and `{class=..}` blocks, `(comments)`, quoted strings, separators ` `, `,` and tab; an `ignore-x` turns a plain `ignore` off; `E` and four digits only; an unknown edition is none) |
| hidden lines: `# x` is code, `##` is `#`, a lone `#` is an empty line | `html/markdown.rs` `map_line` | `markdown.rs` `tested_line` |
| `#![doc(test(attr(..)))]` and `#![doc(test(no_crate_inject))]` | `doctest.rs` `scrape_test_config` | `doctest.rs` `OfTheCrate::read` |
| the example's inner attributes (lints to the module, the rest to the crate), its `extern crate`s and the text between, `fn main` found among items or written by a macro (a macro call statement only in braces or with its `;`, rustc's `StmtKind::MacCall`; one in parentheses with no `;` is an expression), a lone `;` kept with the rest, anything but items means no main of its own, a parse error compiles it as written | `doctest/make.rs` `parse_source`, `check_item`, `DocTestBuilder` | `doctest.rs` `Parts::of`, `statement`, `writes_main` |
| the program: `#![allow(unused)]` or the crate's test attributes, the example's crate attributes, module attributes and crates, an injected `extern crate r#lib` when the text names the library and has none, a `fn main` wrapping `_doctest_main_{id}` (returning a `Result` when the example ends with `(())`) unless the example has one, uses `test_harness` or is `no_std` | `doctest/make.rs` `generate_unique_doctest` | `doctest.rs` `Pieces::of`, `Pieces::rustdoc` |
| the program's id: the file with every non-alphanumeric as `_`, the line, the example's number at that line | `doctest.rs` `generate_test_id` | `doctest.rs` `programs` |
| edition: the example's `edition20xx`, else the library's | `doctest.rs` `run_test` | `doctest.rs` `edition_of`, `Program::of` |
| ignore for a target: `ignore-x` matches when the target triple contains `x` | `doctest/runner.rs` `add_test` | `doctest.rs` `Program::of` |
| line offset: the lines the program says it adds before the example's (its `extern crate` counted as one line though it writes two), which rustc is told to move the example's lines by | `doctest/make.rs` `generate_unique_doctest`, `doctest.rs` `UNSTABLE_RUSTDOC_TEST_LINE` | `Program::line_offset`, `Pieces::aligned` |
| verdicts: `compile_fail` passes when refused with each error code (`error[E..]` in the errors), any other fails when refused ("Couldn't compile the test."), `no_run` passes once built, `should_panic` passes on any failing exit, any other on success; rustdoc's words for each | `doctest.rs` `run_test`, `TestFailure` | `doctest.rs` `Program::after_build`, `runner` |

## How it runs (the design)

1. **Planning.** blaze-units plans one `Mode::Doctest` unit per library under `blaze test`, as
   cargo does (`graph.rs`), after the library and its dev-dependencies. `CompileMode::of` gives
   it `CompileMode::Doctest`, its outputs are a program's (`outputs.rs`) and it takes no lints
   of the package (rustdoc runs examples with the crate's test attributes only).
2. **Expansion in the daemon.** The examples are known only once the library's front end has
   read its docs, so the daemon expands the `Doctest` unit into one unit per example after the
   library's def map is built: `blaze_doc::doctest::examples` then `programs` gives each
   example's `Program` (name, id, source, edition, flags). The expansion is a query keyed by
   the library's docs and its test attributes; adding, removing or editing an example adds,
   removes or patches that example's unit only.
3. **Each example's crate.** A crate whose root is a synthetic file holding `Program::source`
   (added to its source root as OUT_DIR files are, blaze-lower `set_files`), with the edition
   of the program, the library's cfgs (cargo's feature `--cfg`s) but neither `doc` nor
   `doctest` (rustdoc sets those only to collect), and the library and its dev-dependencies
   as dependencies. A `test_harness` example is built as rustdoc's `--test` builds it: a test
   crate with `cfg(test)`, its root given libtest's harness module as a test target's is
   (`HirFrontend::harness`, appended through `set_appended_root_module`), so its `#[test]`s
   run under its own harness and it needs no `main`. Its identity covers the program's source,
   the library and its flags (`keys-cover-every-input`).
4. **Build and run.** The crate is built and linked as a test binary is, patched in place on
   an edit, and run in its own process by the test runner, sandboxed like tests. The runner
   reports it under the libtest name rustdoc gives it, and judges it as rustdoc does:
   `ignore` is reported ignored and not built, `no_run` is built and not run, `should_panic`
   passes only on a panic exit, `compile_fail` passes only when the build fails, and with error
   codes only when each code is among the errors.
5. **Lines.** A panic or diagnostic in the program is reported at the doc's line, in the
   documented file, as rustdoc has rustc report it (`UNSTABLE_RUSTDOC_TEST_LINE` and
   `UNSTABLE_RUSTDOC_TEST_PATH`): blaze needs no remapping, because `Program::source` already
   puts each line of the example's rest on the line rustdoc shows it at
   (`Pieces::aligned`: what rustdoc adds is joined onto as few lines as it fits, then padded
   with newlines), and the program's file is named `Program::file`. One known difference:
   `line!()` in an example gives the shown line, where rustdoc's gives the program's own.
6. **The runner.** A library's examples are run by one generated libtest harness
   (`blaze_doc::doctest::runner`), the `Doctest` unit's executable, which the client already
   runs and heads "Doc-tests" (`cli/test.rs`): one `DynTestFn` per program under rustdoc's
   name, sorted as rustdoc sorts, `ignore` reported ignored; each test reads its build verdict
   from `doctests/verdicts` beside the runner (`verdict_line`, written by the daemon after it
   checks each example, since a phase can't write the database) and, for `run`, starts
   `doctests/<id>` in its own process and judges its exit. No example is merged with another.

A unit blaze can't build fails alone: by default every other unit builds and its tests run,
and the run fails, naming the unit and the tests it held back; under `--fail-fast` the run
stops, naming the unit.

## What is built (2026-11-02)

- The up-front refusal is gone: `CompileMode::of` plans `Mode::Doctest`
  (`crates/blaze-units/src/load/plan/hash.rs`), with a program's outputs
  (`load/plan/outputs.rs`), no package lints and the target's runner (`load/plan.rs`).
- `blaze test` and `blaze bench` keep going by default (`--fail-fast` opts out): the tests of
  every unit that built run past a failed unit, then the run exits 101
  (`crates/blaze-cargo/src/cli/args.rs`, `cli.rs`).
- `crates/blaze-doc/src/doctest.rs`: every rule in the table above, with unit tests of each
  (names, ids, sources, parts, mains, attributes, a fixture crate's examples and lines).
- Lane doctest2 (2026-11-02), diffed against rustdoc at the pinned nightly's commit
  (16d1ffac17ad): the info string parser is rustdoc's tag iterator (`markdown.rs` `tokens`);
  macro call statements follow rustc's `MacCall` rule and a trailing `;` stays with the rest
  (`Parts::of`, `statement`); `Program` carries `file`, `line` and the line-aligned `source`
  (`Pieces::aligned`); rustdoc's verdicts (`Program::after_build`) and the runner's source
  (`runner`, `verdict_line`). A `Doctest` unit's crate has `cfg(doc)` and `cfg(doctest)`
  (blaze-lower `crates.rs` `cfg`), as rustdoc collects.

- Lane doctest3 (2026-11-02): the daemon expansion, the runner and `blaze test --doc`.
  - The loop's writer (`serve/unit/harness.rs` `write`) makes a `Doctest` unit's examples
    into crates once its proc macros are loaded (`serve/unit/compile/doctest.rs` `expand`,
    blaze-lower `doctests.rs` `Lowered::doctests`): one bin crate `rust_out` per example not
    ignored, and one for the runner, each in a source root of the unit's own at
    `<doc file>.<id>.doctest.rs`, named in `Lowered::paths` by the documented file (so its
    errors and panics point there); the `Doctest` crate's deps, its cfg without `doc` and
    `doctest`, the program's edition. A later write reuses each crate and file by id and
    sets only what changed. The crates go in `Job::doctests`.
  - `codegen::generate` and `codegen::link` take the crate (`krate`) beside the unit, every
    caller ported.
  - The unit's phases (`compile/doctest.rs`): its record is replayed when kept; else each
    example is checked, judged (`Program::after_build`) with its errors rendered against its
    own text, and its code compiled unless it is `compile_fail`; the runner's too. The link
    puts each example at `<runner>.doctests/<id>`, the runner at the unit's program and the
    verdicts at `<runner>.doctests/verdicts`; `store` keeps those outputs with the record, and
    a kept image of an example is relinked at its own path (job.rs `Paths::extras`).
  - `blaze test --doc` (blaze-units `load.rs`): the doctest units of the selected libraries
    only; `harnessed` gives them the std `test` crate.
  - `#[doc(test(attr(..)))]` on modules and items (hir-def `AttrFlags::doc_test_attrs`, read
    down each example's path in `examples`) and skipping items, with what is inside them,
    whose `#[target_feature]`s the target lacks. rustdoc leaves a skipped item's attributes
    on the path for the items after it (it does not truncate); blaze truncates.
  - The proof (one ix VM, results in `/tmp/blaze-lanes/v2-doctest3.md`): see the lane's
    handoff for the fixture's and smallvec's outcome tables against `cargo test --doc` and
    the edit check.

- Lane doctest4 (2026-11-02): per-example patching, parallel examples, `test_harness`.
  - An edit to one example changed every example's file: each relink staged a new image
    generation, and the generation is in the image id written as `.note.gnu.build-id`, so an
    example whose rows, names and inputs were all the same still got new bytes. A link that
    keeps every row and is given the same names, debug names, span tables, cold set and natives
    as the last (`codegen.rs` `given_id`) is now not staged: the image's committed file is
    given back as is (blaze-link `Image::file`, span "link unchanged"). An edit to one
    example now changes only that example's file.
  - The examples are checked, compiled and linked side by side through `blaze_graph::par::map`
    (`compile/doctest.rs` `generate`, `link`), each under spans "doctest example",
    "doctest example check", "doctest example codegen" and "doctest example link"; the link
    has spans "link natives", "link inputs" and "link perf map".
  - `test_harness` examples are test crates with libtest's harness appended (step 3 above);
    blaze-lower `DoctestProgram` has `test` and `appended`.
  - The proof is in `/tmp/blaze-lanes/v2-doctest4.md`.

- Lane doctest5 (2026-11-02): an example nothing it is made of changed costs one lookup.
  - Each image (an example's, the runner's) is keyed before it is checked or built
    (`compile/doctest.rs` `base_key`): the unit's record inputs without its crate identity
    (profile, platform, flags, toolchain, blaze; `unit_part`), the example's crate's code
    identity and its id. Its record (`ImageRecord`: a unit record's header naming the output
    and the native inputs the link read) is filed as a unit record is, under what its code
    read (`reads::file`), and found the same way (`filed_in`, the header's check that the
    output is in the store and each native input is as it was). A found image is not
    checked, compiled or linked: its output is placed from the store (`Written::Kept`) and it
    is reported `unchanged`, under one span "doctest image key".
  - The code identity (blaze-lower `code.rs` `code_identity`) is the crate identity with each
    comment of a workspace file that no macro can see replaced by its shape (its line count,
    and a block comment's last line's bytes and characters), so an edit of one example's doc
    text leaves the library's code identity, and every other example's key, as they were. A
    doc comment a macro is given keeps its text: inside a token tree or macro definition, or
    on or under an item given to an attribute macro or a derive that is not one of the
    standard derives; `derive(Debug)` and the like, `test` and `bench` count as macros where a
    file could shadow them. A crate whose code may read a file's bytes as text that could be
    Rust (`include_str!`/`include_bytes!` not of one non-`.rs` literal), or gives an
    out-of-line module to an attribute, keeps its whole text. Unit tests: `code/tests.rs`.
  - Each example's crate has a source root of its own (blaze-lower `doctests.rs`), so an
    example's crate holds no other example's file; the root is set again when a view of the
    package's files (set on an edit) took its file out, as it takes out every file under the
    package's directory it lacks.
  - blaze-api schema 31: `LinkHow::Unchanged`, shown as `link: <path> (unchanged)`; a link
    staged with nothing to stage (`codegen.rs` `Laid::Nothing`, span "link unchanged") is
    reported so too, not as spliced. `harness/bench.py` counts it as spliced.
  - A `test_harness` example's file is written once in `expand`: again (span "doctest
    harnesses written again") only when the harness made from the plain crate differs.
  - Proof (VM doctest5, smallvec 1.13.2, `from_buf`'s example edited at line 857): on the edit
    build the 12 other examples and the runner are `unchanged`, each 2.1 to 2.3 ms in all
    (one "doctest image key" span, no check, codegen or link span), against about 850 ms of
    check and codegen and 220 ms of link each before; the edited example is spliced and 13/13
    pass. Numbers and the mutation check are in `/tmp/blaze-lanes/v2-doctest5.md`.

## What is left, in order

1. The edited example's own codegen takes about 1.1 s of wall time with 82 ms of its own CPU
   (span "doctest example codegen"): a wait no finer span names yet. The same build reruns
   unit bodies (442 ms) and unit codegen (467 ms) spans, most likely the library's, whose
   text the doc edit changed though its code identity did not. Name the wait, then key the
   library's work by its code identity too.
2. Extraction details: items under `#[cfg(doctest)]` (rustdoc builds the library with
   `cfg(doctest)` when collecting), docs from `#[doc = include_str!(..)]` (the line is the fence
   line in the included file), `#[doc(hidden)]` items (rustdoc still tests them), re-exports
   (rustdoc tests an item where it is defined, once), and `#[doc(test(attr))]` on fields of
   extern blocks and items inside function bodies, which the walk does not visit.
3. The outcome comparison on more crates with many doctests (`itertools`, `regex-syntax`,
   `blaze-hash`), each example's outcome against `cargo test --doc` at the pinned nightly.
