---
status: plan
summary: frontend-fast-bodies-cache — a dependency's check computed once for everyone: its crate-level outcome (diagnostics as the build shows them, failed or not) and, per body, the inference result and borrowck verdict codegen reads, in stable form, filed beside frontend-deps' image under the image's key and what else the check reads.
date: 2026-10-29
---
# frontend-fast-bodies-cache: a dependency checked once for everyone

Part 3 of [fast-bodies.md](fast-bodies.md) (section 5 of [fast.md](fast.md); compile-time.md
idea 8). Costs are counted from the code as of 2026-10-29, not measured: nothing builds yet.
Machine figures: the top of [../db/speed-of-light.md](../db/speed-of-light.md).

Two values per dependency, both beside frontend-deps' image ([deps.md](deps.md)) in the same
store (`hir_def::deps::ImageStore`, which is the local store or the local store over the global
cache, `deps::ReadThrough`):

- **The outcome** (`crates/blaze-diagnostics/src/cache.rs`, this lane): what
  `blaze_diagnostics::check_crate` gives for the crate, as `Diag`s, before rendering, so colour,
  `--short` and the unit's name are applied at replay exactly as after a check.
- **The bodies** (`crates/hir-ty/src/check_cache.rs`, child `frontend-fast-bodies-cache-bodies`,
  its notes [fast-bodies-cache-bodies.md](fast-bodies-cache-bodies.md)): per body of the crate's
  items, the `InferenceRecord` with every id relocated through a table (as the signature records
  are, `deps/signatures/table.rs`), and whether `hir_ty_borrowck::checked` passed it.

## The work and its bound

- **A hit, per dependency crate.** Hash the key (about 40 bytes of ids plus the lint flags,
  about 100 bytes: under 0.2 µs with BLAKE3), one store read of the outcome (a dependency built
  with `--cap-lints allow` that compiles has no diagnostics: the value is 12 bytes) and one of
  the bodies part (read in place, opened by checking two plain columns). Bound: two store gets,
  about 2 × 1 µs from the mapped local store, overlapped across crates on the store's
  `for_each` pool; the decode of an empty outcome is a header compare. Against it, checking the
  crate: serde's 1,900 bodies at about 30 µs of inference each plus borrowck and the lints is
  about 100 ms of CPU (60 ms of inference, [../db/speed-of-light-infer.md](../db/speed-of-light-infer.md)).
- **A served body, as codegen asks for it.** Find the record (binary search of the part's key
  column, 11 compares for 2,000 bodies, about 0.1 µs), decode its table (one portable id per
  distinct definition the body names, each re-interned: about 50 ns each, 20 to 60 per body) and
  rebuild the result (`InferenceRecordRef::to_result`: each distinct type interned once). For a
  body of 5 KB with 100 types: about 6 µs, against about 30 µs to infer it. The bound is the
  interning, which no design avoids while the result holds this database's interned types.
- **A miss.** The check runs as today (sibling `frontend-fast-bodies-tasks` schedules it), then
  one encode of the outcome (a walk of the diagnostics) and of the bodies (one record per body,
  each already the `InferenceRecord` the database may keep for eviction): the work over the
  check is under 2% (an encode is a copy of the record's columns and a table per body).

## The key

Agreed with frontend-deps (2026-10-29, tmux to `frontend-deps-next2`; requests in
`v2/requests/frontend-deps.md`).

- **The image's key as it is filed** (`hir_def::deps::Image::key`): the crate's key
  (`deps/key.rs`: content, files, dependencies' keys, toolchain, `build`, paths relative) and,
  for a crate with reads, the values those reads had (`deps/reads.rs`: the process variables
  `env!` read, what proc-macro expansions read). The check reads all of that: a key from the
  crate key alone would serve an outcome after an `env!`'s variable changed.
- **The check key**, per crate and without lint flags. It is
  `cache::bodies_key(image, deps, borrowck)`, filed by what the crate's bodies read:
  - `deps` is each dependency's own check key, in the crate's dependency list order. A body
    evaluates its dependencies' constants and `const fn`s. The image key covers their image
    keys, not what their bodies read. A crate one of whose dependencies has no check key has none
    either (`cache::Refused::Dependency`). Serving and storing therefore go through the crate
    graph in layers, a crate after its dependencies.
  - The image's reads cover module scopes and signatures only, never the macro calls in bodies,
    which the check expands (an `env!` of a process variable in a fn body, a proc macro or
    `include!` there). After the check, `hir_def::deps::checked_reads` adds every body's macro
    calls. The names go in a slot under the base key and the bodies part under the key their
    values make (`deps::file_by_reads`). Serving asks the base key, then each slot with its
    values now (`deps::find_by_reads`), so a changed value is a miss. A body expansion whose
    reads are unknown is a refusal (`cache::Refused::Reads`). Asked of frontend-deps under
    `## frontend-fast-bodies-cache: values made from bodies, filed by their reads`.
  - The bodies part is under the check key, and frontend-fast-const-deps files a crate's constant
    values by it (`Served::keys` at session start, `Stored::keys` at drain;
    `v2/requests/frontend-fast-const-deps.md`).
- **The outcome's key** is `cache::key(check, inputs)`: the check key and what the check reads
  beyond it, listed next.
- **What the check reads beyond the image** (`cache::Inputs`): the lint flags rustc is given,
  in order (`blaze_cargo::serve::unit::compile::lint_flags`: cargo's `[lints]`, the lint flags of
  `RUSTFLAGS`, `--check-cfg`, `--cap-lints`), `Options::keep_duplicates`, and the borrow
  checker's mode (`hir_ty_borrowck::mode()`, from `BLAZE_BORROWCK`, which `hir-ty-wfcheck` reads
  too). Every other process variable the check code reads is a debug printer
  (`BLAZE_BORROWCK_DEBUG`, `SOLVER_DEBUG`, `RUSTD_DUMP_ERRORS`, `RA_CHECK_RELATIVE_BODIES`:
  found by `rg 'env::var'` over every crate the check runs) and changes no outcome.
- **The producer**: the outcome's encoding version (`cache::PRODUCER`). The check's code is in
  the image key already (`build`, the blaze that reads).
- **The workspace root is not an input**: a diagnostic's paths are kept relative to the crate's
  package directory and `OUT_DIR` and named from the reader's root as `Files::relative` names
  them, so a checkout at another path gets what its own check would show.
- `cache::key` is the one function that makes it; the global cache's rule makes it again with it
  (`blaze_global::public`, fastcompile).

Tests (`cache/tests.rs`): `the_key_changes_with_each_input` (image key, each lint flag, their
order, `keep_duplicates`, the borrowck mode); `a_dependents_text_does_not_change_the_key` (a
database where a crate depending on the dependency is edited gives the dependency the same image
key, so the same outcome key).

## The value

- **Outcome** (`cache::Outcome { failed, diags }`): encoded with the `Diag` model's serde (it is
  the model the build renders and `blaze_api` sends), behind a 16-byte header (magic, version,
  count). Before encoding, every path and text is made portable: the package directory and
  `OUT_DIR` become markers; a span's file is first made whole from the writer's root. An outcome
  holding any other absolute path of the machine (a file of another crate in a macro's
  `def_site`, the workspace root in a message) is refused (`cache::Refused::MachinePath`): it would
  not read the same elsewhere. Decoding is bounded (the store checked the hash; the header's
  count and the serde decoder's depth limit bound the rest), and a value that does not decode is
  an error naming the crate and key.
- **Bodies**: the child's part. MIR is not stored by either side: agreed with lane codegen
  (`codegen-mir-store-next2`, 2026-10-29): no MIR persistence exists (`rg 'Persist for MirBody'`
  finds none) and the code hash is computed from MIR (`hir-ty-codegen/v2/hashes.rs`), so a
  consumer lowers MIR from the served inference and body; if codegen later keeps MIR by content,
  it is on its side, keyed by the code hash, and this part stays as it is.

## The hook

- **Serve** (`cache::serve`, the daemon's writer, right after `hir_def::deps::serve` and with the
  same adapter): for each crate served from an image, the outcome and the bodies part are read
  in parallel (`ImageStore::for_each`); the bodies are given to the database
  (`hir_ty::check_cache::set_served`), the outcomes kept by the job for its units. Like
  `hir_def::deps::serve`, it adds to what was served before (`&mut Served`, one per session):
  the driver asks crates as their inputs become final, a crate after its dependencies, and a
  crate asked again under the keys it was looked up under is no lookup. A crate asked under other
  keys is fetched again and loses what it had on a miss.
- **A unit** (lane cargo's `make()`): a crate with a served outcome renders it and runs no
  `check_crate`; codegen then reads served inference (the hook in `infer_body_query`) and the
  served verdict (the hook in `hir_ty_borrowck::gate::checked`). A crate without one is checked,
  and the outcome kept for the store step.
- **Store** (`cache::store`, one call on the loop after `hir_def::deps::store`, which gives each
  crate imaged this session its filed key): the bodies and the outcome of every crate checked
  this session that has an image, in layers of the crate graph so that a crate's dependencies'
  check keys are known first. `make()` only records what it checked.
- **Refused here when frontend-deps refuses the image**, for the same reason: the outcome's key
  is the image's, and a crate with no image has none (`cache::Refused::NoImage`, shown under `-v`
  beside `deps::Stored::refused`, which says why: proc-macro items whose reads are unknown,
  session macros, a file outside the source root).
- **Never a quiet re-check**: an outcome under its key that does not decode, or a bodies part
  whose record of a body does not read, is an error naming the crate and key.

## What rustc behavior is kept

A served outcome is the diagnostics the check would give, in its order (the encoding keeps the
`Vec` order), and the same failed bit: the build stops at a failed dependency as rustc does.
Paths are re-rooted exactly as `Files::relative` would name them. Nothing about which programs
are accepted changes: a body is served only from a value made by checking that same crate under
the same key.

## Prior art

rustc keeps nothing of a dependency's check but `.rmeta` (signatures and MIR of generic and
`#[inline]` items): every machine checks every crate once. sccache and Bazel's remote cache
share the whole `rustc` invocation's outputs keyed by its command line and inputs, which misses
on any absolute path; this key is path-free, so one builder's check serves every checkout.
Nix's binary cache is the model for "computed once for everyone, trusted by signature"
(blaze-global's `sign`).

## Proof

- `crates/blaze-diagnostics/src/cache/tests.rs`: key inputs and non-inputs (above);
  `an_outcome_reads_back_in_another_root` (encode under one workspace root and package path,
  decode under another: every span names the file as `Files::relative` would there);
  `a_served_outcome_renders_byte_identical` (a fixture crate with an error and a warning: the
  diagnostics `check_crate` gives and those read back render to the same bytes);
  `an_outcome_naming_another_machine_path_is_refused`; `damaged_outcomes_are_refused_within_bounds`
  (every truncation and a flip of each header byte: an error, never a panic);
  `an_edit_to_the_dependency_misses` (the image key moves, so the outcome key does);
  `a_bodys_process_variable_files_the_outcome_by_its_value` (served with the variable as it
  was, missed once it is another);
  `a_served_dependency_reads_its_outcome_and_bodies_in_a_second_database`;
  `an_outcome_under_its_key_that_does_not_open_is_an_error`.
- `harness/v2/frontend/cases/bodies-cache/`: 48 programs, with 44 dependency files in
  `auxiliary/`. frontend-fast-diff's `cases.py` builds each with rustc and twice with blaze, one
  store between, and fails a second build that does not serve each dependency's outcome and
  bodies (the `blaze-served` line, asked of cargo). They cover the shapes a dependency's body
  takes:
  - generics and const generics, closures, `async fn`s, constants, statics and discriminants;
  - trait objects, operators, associated items, lifetimes and patterns;
  - `option_env!` in a body, items in blocks, and native proc macros in a body and as a derive;
  - a three-crate chain of constants and a diamond;
  - dependencies that fail to check (E0308, E0499, E0599, E0425), one that panics, and an
    error in the case beside a served dependency.

  The one hand-over marked `//@ refused:` is an outcome whose note points into another crate's
  file.
- The child's tests ([fast-bodies-cache-bodies.md](fast-bodies-cache-bodies.md)): a record read
  in a second database means the same; a served body gives the same inference result and the
  same MIR as a checked one; damaged parts are refused within bounds.

## Open

- **No verification mode for bodies yet**: `deps::Mode::Check` re-resolves a served crate and
  records differences; served bodies and outcomes have no such run. Wanted: under `Check`, check
  the crate as well and fail on any difference from the served outcome or a served body's
  inference, run daily by the differential harness.
- **The relocation scope is a thread local** (`hir_ty::check_cache::ids`): a query run inside a
  `relocating` scope that reads another persisted table would read the scope's indexes. The child
  wraps the two known calls in `outside()`; the end state is a reader that takes its table as an
  argument.
- **Diagnostics in id order**: some lists in an inference result (`added_auto_traits`,
  `unstable_candidates`) may be in salsa-id order, so a served body could give its diagnostics
  in another order than a fresh one. The outcome's diagnostics are the crate's, kept in order,
  so the build's output is unaffected; codegen reads no diagnostic of a body.
