# Requests from lane usercode-fast-scripts

## 1. blaze-cargo test: `Env::vars` is a sorted vector (required)

- What: replace
  `assert_eq!(env.vars.get(&OsString::from("DEP_Z_INCLUDE")), Some(&OsString::from(format!("{out_dir}/include"))));`
  with
  `assert_eq!(env.get("DEP_Z_INCLUDE"), Some(OsStr::new(&format!("{out_dir}/include"))));`
  (and `use std::ffi::OsStr;` if the file lacks it).
- Where: `crates/blaze-cargo/src/serve/unit/tests.rs:202`.
- Why: `blaze_build_script::Env::vars` is now `Vec<(OsString, OsString)>`, sorted by name and
  each name once (it was a `BTreeMap`), so the environment is one presized vector sorted
  once. `Env::get` is the binary search. Every other caller compiles unchanged:
  `.vars.iter()`, `.vars.into_iter().collect()` and `Env::apply` read the same order.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/unit/tests.rs:206 (`env.get("DEP_Z_INCLUDE")`), with `OsStr` imported.

## 2. blaze-cargo: judge a run by borrowing (optional, speed)

- What: in `judge` (around line 289), replace building a `blaze_build_script::run::Ran` from
  `value.stdout.clone()` and `value.stderr.clone()` and calling `ran.finish(unit, &cx)` with
  `blaze_build_script::run::judge(unit, &cx, status, &value.stdout, &value.stderr)`, and write
  cargo's files of the run (`run::files(unit, stdout, stderr)`: `output`/`stdout` dated when
  the run began, `stderr`, `root-output`) through `Effect::Write` once `judge` has passed the
  status and logged-error checks, as `finish` does.
- Where: `crates/blaze-cargo/src/serve/unit/script.rs`, `judge`.
- Why: `finish` copies nothing but both clones are whole outputs, and it writes the files with
  `std::fs` on the daemon's thread, which blocks it.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-user): done at crates/blaze-cargo/src/serve/unit/script.rs:335 (`judge` calls `run::judge` on the borrowed value; nothing is cloned) and :529, :667, :693 (`Writing`). Once the run passes, cargo's three files are written through effects: each is cut (`Effect::Truncate`), then given its bytes (`Effect::WriteAt`, moved out of the value), and `output` is dated with the run's start (`Effect::Times`, mem-core-times' answer). None is synced, as with cargo. `Effect::Write` would sync twice per file. The run ends once every effect completes. Tests: `serve/unit/script/tests.rs`.

## 3. blaze-cargo: begin an elided run without cloning its output (optional, speed)

- What: in `ScriptRun::start` (around line 608), call `blaze_build_script::run::begin(&start.unit)`
  for the time the run began, rather than cloning the inert run's stdout into
  `run::elided(&start.unit, stdout)` and keeping only its `.began`;
  better, make `OUT_DIR` and the run directory and write `run::invoked(unit)` (a path and its
  bytes) through effects, and take the time from the `Stat` of that file.
- Where: `crates/blaze-cargo/src/serve/unit/script.rs`, `ScriptRun::start`.
- Why: `elided` only wraps `begin` and the stdout given; the clone is thrown away. `begin`
  calls `create_dir_all` and `fs::write` on the daemon's thread.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-user): done at crates/blaze-cargo/src/serve/unit/script.rs:734 (`ScriptRun::start` calls `run::begin(&start.unit)`; an inert run's value is moved out of its `Start`, not cloned). The "better" variant is not done, because no effect makes a directory: `Effect::Write` makes only a file's parents, and it syncs. `OUT_DIR` must exist before an outside run spawns. A sandboxed run's `Run::begin` already empties and restores `OUT_DIR` on this thread.

## 4. blaze-usercode: read what reruns a script without parsing everything (optional, speed)

- What: in `cargo_rule`, replace `match output::parse(&exit.stdout, &cx)` with
  `match blaze_build_script::watched_in(&exit.stdout, &cx, dir)`, its `Ok` arm becoming
  `Ok(watched) => CargoRule::of(watched)`. It returns the same `Watched`, or the same
  `Error` as `parse`.
- Where: `crates/blaze-usercode/src/script.rs`, `cargo_rule` (around line 111).
- Why: `watched_in` reads the output in the same pass and keeps only the `rerun-if-*` values,
  borrowed; `parse` copies every directive into an `Output` that is then dropped.

> forwarded to usercode (told by tmux).

> applied (usercode): `script::cargo_rule` calls `blaze_build_script::watched_in`.

## 5. docs/README.md: index the new plan doc (required)

- What: run `python3 tools/docs.py write` so the index lists
  `docs/v2/plan/lanes/usercode/scripts.md` and `docs/v2/plan/lanes/usercode/speed-of-light.md`.
- Where: `docs/README.md` (generated).
- Why: the docs are new, and `tools/docs.py check` fails on a stale index. (`speed-of-light.md`
  is lane usercode's, with a section per component; this lane wrote the first, for
  `crates/blaze-build-script/`.)

> forwarded to the coordinator: `docs/` is its; listed in requests' report (run `python3 tools/docs.py write`).

## 6. blaze-usercode, blaze-cargo: pass the inherited environment sorted (optional, speed)

- What: sort the environment the daemon (or cargo's process) inherits once per build, by
  name with each name once (`sort_by(|a, b| a.0.cmp(&b.0))` then `dedup_by` keeping the last
  word, which is what `env` keeps), and pass that slice to every `blaze_build_script::env`.
- Where: `crates/blaze-usercode/src/script.rs` (`prepare`, the `inherited` it is given, and
  whoever makes it), `crates/blaze-cargo/src/cargo/compile_time.rs` (`self.inherited`).
- Why: `env` now checks whether the inherited environment is sorted (one pass) and only
  sorts its indices when it is not: about 3,600 cycles a unit saved, a quarter of `env`'s
  bound (`docs/v2/plan/lanes/usercode/speed-of-light.md`, "The environment"). The result is
  the same either way.

> forwarded to usercode and cargo (told by tmux).
> answered (usercode): nothing to change in `blaze-usercode`. `script::prepare` and `cargo_rule` pass the slice they are given straight to `blaze_build_script::env` and `output::Context::of`, and cargo makes it (`crates/blaze-cargo/src/serve/unit/script.rs`, `inherited`). Sorting it there, once per job, is cargo's side.

## 7. `Output` and `Env` as one buffer with spans (optional, speed, needs the callers' lanes)

- What: `blaze_build_script::Output`'s string fields and `Env::vars` become views into one
  owned buffer (spans of `u32` offsets, read through methods returning `&str` and `&OsStr`),
  in place of a `String` or `OsString` per value.
- Where: `crates/blaze-build-script/src/output.rs` and `env.rs` (this lane), and every reader
  of `Output`'s fields and `Env::vars` in `crates/blaze-cargo/src/serve/unit/` (`script.rs`,
  `compile.rs`, `codegen.rs`), `crates/blaze-cargo/src/cargo/compile_time.rs` and
  `crates/blaze-usercode/src/script.rs`.
- Why: allocations are the largest term left in both bounds: 400 of `parse`'s 20,750 cycles
  for a heavy script's output are 8,000, and 300 of `env`'s 11,050 are 6,000. This lane does
  its side once the owners of those callers agree to the shape; until then, a caller that
  needs part of an output reads it with its own `output::Sink`.

> forwarded to usercode, cargo (the callers; told by tmux).
> agreed (usercode, as a caller): `script::prepare` reads only `Env::vars`, collected into owned pairs for the run (`script.rs` line 75); it will read the spans through the methods instead, in the same change. `blaze-build-script` is usercode's, so lane usercode makes that change once cargo agrees for its callers.

## 8. blaze-cargo: read what a unit links with through `to_link_in` (optional, speed)

- What: call `blaze_build_script::apply::to_link_in` (which lends the memo's entry) where
  `to_link` is called, and read `to_link` and `plugins` through the reference.
- Where: `crates/blaze-cargo/src/serve/unit/graph.rs` (line 253, the one caller of
  `to_link`).
- Why: `to_link` clones the memo's `BuildScripts` (two collections) for every unit asked;
  `to_link_in` allocates nothing on a memo hit, which `a_unit_met_again_is_free` holds it
  to. `to_link` stays, as the owned copy, until no caller wants one.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/unit/graph.rs: `links.extend_from_slice(&to_link_in(at, &node, &mut memo).to_link)`, no `Vec` per unit.

## 9. Cargo.lock: blaze-build-script's dependencies changed (required)

- What: drop `blaze-ids` from `blaze-build-script`'s dependencies and add `blaze-mem` and
  `blaze-alloc-count` (a dev-dependency) in `Cargo.lock`, as the next resolve does.
- Where: `Cargo.lock`, the `blaze-build-script` package entry.
- Why: `blaze-ids` is no longer used (its `radix` was the last use, gone from `apply.rs`);
  `blaze-alloc-count` counts allocations for this crate's allocation-budget tests. The
  manifest lint passes.

> applied: listed under lock additions in requests' report, for the first compile (requests edits no lock).

## 10. tools/v2/hot_lint.py: `crates` as a tuple (required, the lint fails without it)

- What: declare the input's `crates` field as `tuple[str, ...]` (default the `HOT` tuple)
  in place of `list[str]`, and skip a file whose first item is `#![cfg(test)]` as a
  `#[cfg(test)]` block is skipped.
- Where: `tools/v2/hot_lint.py`, its input dataclass.
- Why: `typed_input` refuses a `list[str]` field ("crates has a type it does not read"), so
  `{"root": ".", "crates": ["blaze-build-script"]}` fails before reading a file. This lane
  ran the lint's own `lint_file` over the crate, with the same skips, to reach 0 findings.

> forwarded to the coordinator (`tools/`), listed in requests' report.

## 11. Callers: what changed in blaze-build-script's API (for information)

- `Clone` is gone from `Env`, `run::{Invocation, Ran, Failed, Files}`, `Watched`,
  `apply::{Compile, Rerun, Stale}`; nothing in the tree cloned them. `Output`, `Unit`,
  `Package` and `BuildScripts` keep it: blaze-cargo's `serve/unit/script.rs` `Applied`
  derives `Clone` with an `Output` in it, and `to_link` returns a copy. If `Applied` need
  not be cloned, its owner can drop that derive and this lane drops `Output`'s.
- `Compile::args` returns `Vec<Cow<'_, str>>` (borrowed but for paths and `link-arg=`); its
  one caller is this crate's tests.
- `to_link` takes the graph as a generic, not `dyn`; an existing call compiles unchanged.
- `Rerun::stale` takes lookups returning `Option<&str>` (were `Option<String>`); it has no
  caller outside this crate.
- New `elide::elide_into(root, files, out) -> bool`, printing into a reused buffer; `elide`
  is unchanged. `elide::lex::normalize` takes a `&Frame` and returns `&str` (no outside user).
- `output::parse_rustc_flags` and `output::parse_rustc_env` are gone (no user outside this
  crate; the second lives on as the reference in `output/tests/reference.rs`).

> forwarded to cargo and usercode (information; told by tmux).
> answered by lane cargo (cargo-inbox-user): done at crates/blaze-cargo/src/serve/unit/script.rs:70: `Applied` derives only `Debug` now, since nothing clones it. Nothing in `serve/` uses the `Clone`s that went. `Output`'s `Clone` may go as far as `serve/` is concerned; `to_link` is called through `to_link_in` (item 8).
> read (usercode): usercode's only callers outside the crate are `script.rs`'s `elide::flags_allow` and `elide(root, files)`, both unchanged; none of usercode's code clones the types that lost `Clone`.

> answered by lane cargo (cargo-inbox): read: nothing in `crates/blaze-cargo/` clones the types that lost `Clone`. `Applied` (`crates/blaze-cargo/src/serve/unit/script.rs:44`) is cargo-inbox-user's to drop its derive, and that will be noted in this section when it lands.

## 12. blaze-mem: a path scratch, and the frame for a read root file (optional, mem lane)

- What: a `scratch::with_path` shelf lending a thread's `PathBuf` empty, as `with_string`
  lends a `String`; and, once tasks carry a `Frame`, a way for a caller to hand elision one,
  so its printed text and argument list are bumped, not allocated.
- Where: `crates/blaze-mem/src/scratch.rs` (lane mem); then `blaze_build_script::apply::Rerun::stale`
  and `elide::elide` (this lane) take them.
- Why: `Rerun::stale` now takes the caller's path buffer, which is the design's scratch kept in
  worker state; a shelf would let it take none. Elision makes three allocations per root
  file read (`what_elision_allocates`), all for that one task.

> forwarded to mem (told by tmux).

> answered (mem, mem-codegen, 10-29) and applied (usercode-fast-scripts): `Rerun::stale` joins
> paths in `blaze_mem::scratch::with_path` and takes no buffer; elision runs in
> `frame::task(Tag::Usercode, ..)` with its temporaries in the frame and lent strings, and
> prints into the caller's buffer through the new `elide_into` (requests 2 and 3 of
> `v2/requests/mem-codegen.md`).
