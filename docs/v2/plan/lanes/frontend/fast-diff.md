---
status: superseded
summary: frontend-fast-diff — the proof against rustc (fast.md section 9). Primary spans in rejections.py, the case runner (harness/v2/frontend/cases.py) that holds each case's header to the pinned rustc and then compares blaze's outcome, codes, primary spans and run with rustc's, the seeded fuzzer (fuzz.py) whose mismatches are shrunk C-Reduce style into new cases, and the name resolution and expansion cases.
date: 2026-10-29
superseded_by: ../../../../harness.md
---
# frontend-fast-diff: the proof against rustc, and fuzzing

Area: [fast.md](fast.md) section 9. The bar is decisions.md row 39's: every program the pinned
nightly accepts, blaze accepts with the same meaning; every one it rejects, blaze rejects with
the same error codes at the same primary spans. These scripts measure that bar. They run once
compiling is allowed, and every sibling's cases go through them.

## What is built

| file | what it does |
|---|---|
| `rejections.py` (deleted 2026-10-30) | adds primary spans: `Span`, `place`, `primary_spans`, `span_mismatches`, the category `other spans`; rustc's answers kept with their spans (`rustc-spans`); `run_batch(command="build")`; `rustc_argv(emit, output, extra)` |
| [frontend/cases.py](../../../../../harness/v2/frontend/cases.py) | the case runner; `Runner.compare` is the one comparison that fuzz.py uses too |
| `frontend/fuzz.py` (deleted 2026-10-30) | the seeded generator, the shrinker, and the cases it writes to `cases/fuzz/` |
| [frontend/cases/nameres/](../../../../../harness/v2/frontend/cases/nameres) | section 1's cases (72): E0425, E0432, E0433, E0412, E0659, E0603, E0252, E0255, E0364, E0405, E0411, E0422, E0423, E0428, E0434, E0531, E0599, `cannot find macro`; glob shadowing and glob cycles; macro-expanded `use`; `macro_rules!` textual scope and `#[macro_use]`; `$crate`; 2015 against 2018 paths and the 2021 prelude; a directory module |
| [proof.py](../../../../../harness/v2/proof.py) | runs `cases` and `fuzz` as checks, like the others |

Each script takes one JSON input parsed into a frozen dataclass that refuses unknown fields
(AGENTS.md rule 17). Each has a `selftest` that needs neither blaze, rustc nor the network.

## The comparison

A case is `cases/<area>/*.rs` whose first line is `//@ expect: accept` or
`//@ expect: reject E0xxx[, E0yyy]`. An error with no code is named by the start of its message
in double quotes. Other compiletest directives (`//@ edition:`, `//@ compile-flags:`) are read
the way rejections.py reads `tests/ui`.

1. **rustc first.** A library gets `--emit=dep-info,metadata`, which is what `cargo check` asks
   for. A program gets `--emit=dep-info,link -C debug-assertions=on` (cargo's dev profile, which
   sets overflow checks and `debug_assert!`). When it builds, it runs for at most 10 s, and the
   runner records its exit status, stdout and each panic by thread, place and message. If
   rustc's answer doesn't match the header, the case is at fault (`case wrong`) and it never
   goes to blaze.
2. **Then blaze**, in workspaces of up to `batch` cases of one kind and one set of flags:
   `blaze check` for a library and `blaze build` for a program. A program blaze builds runs the
   same way rustc's did.
3. **Verdict.** A case passes when both sides give the same outcome:
   - rejected with the same set of keys, and each key's primary spans equal as a multiset;
   - or accepted, and for a program the same exit status, the same stdout and the same panics.

   Otherwise it fails as one of: `accepted what rustc rejects`, `rejected what rustc accepts`,
   `other codes`, `other spans`, `other run`, or `not blaze's` (the check was done by something
   other than blaze).

Spans are compared by place. The root file is `<root>`, a module file is its path relative to
the root's directory, and a standard library file is its path under `library/`. This way rustc's
`t.rs` and blaze's `crates/u0_3/src/main.rs` compare equal.

## Cases with a dependency, and cases built twice

Asked for by `frontend-fast-bodies-cache` ([fast-bodies-cache.md](fast-bodies-cache.md)).
Its check cache serves only crates read from a dependency image, and a one-unit case never
takes that path.

- **The directive.** A case names a crate with `//@ aux-crate: dep=dep.rs`, compiletest's
  directive. The file sits in `auxiliary/` beside the case. A dependency may name its own the
  same way. It may take `//@ edition:` (else 2015, as compiletest) and
  `#![crate_type = "proc-macro"]`.
- **rustc's side.** rustc builds each dependency whole, in order, with `--cap-lints allow` and
  the dev profile's flags, as cargo builds a registry crate. The case gets them with `--extern`.
  A dependency rustc rejects is the answer, because cargo stops there too. The kept answer
  covers every file any of these runs read.
- **blaze's side.** Each dependency is a crates.io package vendored in the workspace's `vendor/`,
  with `.cargo-checksum.json`, and `.cargo/config.toml` points crates.io at that `directory`
  source. So each is a registry package (`CrateOrigin::Library`), never a member or path
  package, and `hir_def::deps::eligible` holds for it. Packages are named `u<i>_<j>_<dep>`, one
  a member; `[lib] name` keeps rustc's crate name.
- **Comparing files.** A dependency's root file is compared as `<dep NAME>` on both sides. Its
  other files are compared by their path under `auxiliary/`.
- **`cases/bodies-cache/`.** Each case there is built twice with one store. The second build
  gets a fresh daemon home, workspace and target. Both builds are compared with rustc. Then the
  second's `blaze-served` lines (requested in `v2/requests/frontend-fast-diff.md`) must say both
  the outcome and the bodies of each dependency were `served`. The exceptions are those a
  `//@ refused: dep` names: for each, one part must be `refused`. Those are the edges where the
  cache hands over to a check.

## Which rustc behavior the comparison keeps

- **Codes, not wording.** Diagnostic wording is free (principles.md), so only the code is
  compared. An error with no code is keyed by the first 60 characters of its message, because
  that's all there is to compare. A denied lint is keyed by its name.
- **Primary spans per code, as a multiset.** rustc reports some errors once per use (E0425 at
  every use of a missing name). Its order is not compared, because blaze checks bodies in
  parallel.
- **Run output, in full.** Exit status, every stdout byte, and the panics sorted by thread:
  threads panic in any order. A case must not print anything that names its crate
  (`type_name`, `module_path!`), since the two sides name the crate differently.

## The fuzzer

Program `i` of seed `s` comes from `random.Random("blaze-fuzz:<s>:<i>")` alone, drawing only
`random()` and `randrange`, which Python keeps stable across versions. So a seed gives the same
programs on every machine. Program `i` belongs to area `areas[i % len(areas)]`. A program is 2 to
4 snippets of its area, each printing what it computed. About one snippet in five is broken on
purpose (`BAD = 0.2`) so that some programs are rejected:

- **traits:** blanket impls over `Vec`, `Option`, tuples, arrays and `&X`; auto traits, with
  negative and `unsafe` impls; `Sized` and `?Sized`; associated types; supertraits and
  upcasting; overlap with a local type.
- **methods:** inherent against trait at every receiver; autoderef through `&`, `Box`, `Rc` and
  user `Deref` chains of depth 1 to 4; traits in scope or not; ambiguity; by-value against
  by-reference impls; a trait `len` shadowed by an inherent one.
- **coercions:** reborrows; unsizing (slices, `dyn`, struct tails, upcasting); `!` in `if`,
  `match` and `loop`; closures and fn items to fn pointers (and `unsafe fn`); deref coercion of
  arguments.
- **consts:** every integer type with every operator, in `const`, `static`, `const fn`, at run
  time and as literals. This includes overflow, division by zero, shifts at and past the bit
  width, casts from integers and floats, array lengths, and a generic constant that fails only
  when instantiated.
- **borrows:** two-phase borrows, borrows across loops, moves in match arms, NLL, closures, and
  disjoint fields.

Mismatches are shrunk the way C-Reduce does it: passes run to a fixpoint. Pass 0 deletes an item
or a statement, or empties a block. Pass 1 deletes an element of a list (an argument, a field, a
match arm). Pass 2 deletes a line. A pass's candidates are compared in one batch. The deletions
that keep the same `signature` (the category plus the keys involved) are then applied together,
and the passes start again from 0. The result is written as
`cases/fuzz/<area>-s<seed>-p<i>.rs`, with rustc's answer as its header. From then on it is a
case like any other.

## Speed of light

The harness's own work is negligible beside the compiler runs it starts. Its bound is set by
how many runs it starts and how many it reuses.

- **rustc.** Each case needs at most one run per toolchain. The answer (keys, spans, the program's
  run) is kept under `~/.cache/blaze-v2-proof`, keyed by the toolchain identity, the working
  directory and the argv. It's reused while every file in rustc's dep-info keeps its digest.
  A rerun of N unchanged cases costs N small JSON reads and digests of a few KB of source,
  which is under 1 ms per case against 50 to 150 ms for a rustc run. Runs go `jobs` at a time.
- **blaze.** One daemon per workspace of up to `batch` (100) cases. Daemon start and std loading
  are paid once per 100 cases, not once per case. `whole: true` makes it one request per
  workspace, the fewest blaze can be asked.
- **Shrinking.** Each round is one batch, so a round costs one workspace of candidates. Applying
  every surviving deletion at once (plus one confirming run) is how C-Reduce reaches a small
  program in a few rounds instead of one per deletion. Variants are named by content, so a
  variant tried before reuses rustc's kept answer. `rounds` caps the batches (40).

Prior art: rustc's compiletest (the directives and the JSON diagnostics), C-Reduce (passes to a
fixpoint) and Csmith/YARPGen (seeded generators aimed at one compiler path).

## Not here

- Each sibling's own cases live in `cases/<area>/` and are written by that sibling.
- The scripts have not been run: nothing compiles until every lane is done. Their selftests are
  the first thing to run when it's allowed:
  `uv run --frozen python harness/v2/frontend/cases.py selftest`, then `fuzz.py selftest`,
  `rejections.py selftest` and `proof.py selftest`.
