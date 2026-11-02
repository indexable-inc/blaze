---
status: plan
date: 2026-10-30
summary: no-fallback in blaze-cargo's serve/unit (but codegen and compile) and serve/fetch: each site where a unit's work went on with a guessed value, a dropped edge or a skipped test, and what it is now (typed `UnitError`, serve/unit/error.rs).
---
# no-fallback: blaze-cargo units and fetch

Child `no-fallback-link-cargo-units` of `no-fallback-link-cargo`. Paths: `crates/blaze-cargo/src/serve/unit.rs`,
`serve/unit/` (but `codegen.rs`, `codegen/`, `compile.rs`: the parent's, rows marked "parent's",
not edited here), `serve/fetch.rs`, `serve/fetch/`. Test code skipped. The errors turned up here
are one typed enum, `UnitError` (`serve/unit/error.rs`, with `Display` and `From<UnitError> for
ApiError`); where the surrounding error is still a `String` (`script_unit`) it is formatted at the
site, for later.

Ranked: the daemon's build path (graph, front end kept, build scripts) first, then tests, `run`,
fetch, the rest. Line numbers are after the edits.

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `serve/unit/graph.rs:192` | error | a dependency index past the build's units was filtered out of the CSR: a unit could build before what it needs, silently | `UnitError::Dependency { unit, dep, count }`; `Graph::new` and `Job::new` return `Result`. Done; test `a_graph_with_a_unit_it_cant_place_is_refused` |
| `serve/unit/graph.rs:180` | error | a unit whose package the build doesn't list got package `u32::MAX`, so "same package" checks (own runs, test units) were quietly false | `UnitError::Package { unit, pkg_id }`. Done; same test |
| `serve/unit.rs:408` `UnitWriter::keep` (+ `work.rs:335`, `job.rs:2187`, parent's) | error | a write that fails to apply, or a reader still alive, returns `None`: the front end is dropped and the head's next build lowers anew, its reason lost | `keep` returns `Result` and the build ends with the error. Needs the trait (`work.rs`) and `job.rs`: queued 01166 to no-fallback-link-cargo; `unit.rs` follows when the trait changes |
| `serve/unit.rs:185` | rule | a kept front end lowered from other units or config is not lowered into | stays: the kept database is of other inputs; nothing to be incremental against |
| `serve/unit.rs:199` (`Lowered::refresh` false) | rule | a view that moves the workspace root or its manifest's settings lowers anew | stays: every crate is made with them (blaze-lower's documented rule) |
| `serve/unit.rs:483` | error | a library file's mtime before 1970 hashed as zero: two such times gave the same fingerprint, a stale standard library could be kept | hashed with its sign. Done |
| `serve/unit.rs:433` | rule | `let _ = slot.set(identity)`: an identity set twice keeps the first | stays: set once per unit, every later set is the same value |
| `serve/unit.rs:687` | rule | a kind word other than bin/test/bench/example/custom-build is a library | stays: cargo's rule; a crate type blaze doesn't know is already an error |
| `serve/unit.rs:723` | rule | `fresh` of a unit the graph lacks is false | stays: `compile` then names the missing unit |
| `serve/unit/script.rs:155` | error | no `PROFILE` in a run's plan guessed `release` from the profile's name (wrong for a custom profile inheriting release) | `UnitError::NoVariable { package, name: "PROFILE" }` (blaze-units always sets it). Done |
| `serve/unit/script.rs:169` | error | a dependency's package not found used its extern crate name as the `DEP_*` package | `UnitError::Dependency` / the package error. Done |
| `serve/unit/script.rs:141` | error | a manifest in no directory gave `CARGO_MANIFEST_DIR` = "" | `UnitError::ManifestDir` (shared helper `error::manifest_dir`). Done; test `a_script_run_missing_what_cargo_tells_it_is_refused` |
| `serve/unit/script.rs:180` | error | an `OUT_DIR` under no `<profile>/build` gave no host dylib search path | `UnitError::ProfileDir`. Done; same test (fixture moved under `debug/build`) |
| `serve/unit/script.rs:182` | error | unknown parallelism told the script `NUM_JOBS=1` | `UnitError::Parallelism`, as cargo's `default_parallelism` refuses. Done |
| `serve/unit/script.rs:471`, `script/pure.rs:167` | error | an inert or pure script's program path, with the compile's plan naming no output, was "" | `script::named_program`, `UnitError::ScriptProgram`. Done; same test |
| `serve/unit/script.rs:636` | error | `package_id` of a run whose unit is gone was "" in `build-script-executed` | the run's error. Done |
| `serve/unit/script/pure.rs:122` | error | a build script compile with no crate in the front end was `Unproved` and silently took the build-and-run path | `UnitError::NoCrate`. Done |
| `serve/unit/script/pure.rs:125` | rule | no free `main` with a body: not proved, built and run in the sandbox | stays: proof is of a free `main` only, a rule chosen up front |
| `serve/unit/script/pure.rs:117` | rule | `verifies(job).unwrap_or(true)` | stays: `unbuilt` refused a bad `BLAZE_PURE` first; unreachable, and the value is the conservative one |
| `serve/unit/script/pure.rs:109` | rule | `let _ = slot.set(worked)` | stays: worked out once |
| `serve/unit/script.rs:419`, `:430` | rule | `fresh` treats a failed `prepare` or store lookup as not fresh | stays: `start` prepares again and `Run::begin` (`index::find_held`) returns the same error, which fails the run |
| `serve/unit/script.rs:108` | rule | the inert check's read error is "not inert" | stays: unreachable, a root that fails its check stopped the job at lowering |
| `serve/unit/script.rs:194` | rule | no `CARGO_PKG_AUTHORS` is no authors | stays: optional metadata |
| `serve/unit/script.rs:299` | rule | a Windows end with no code is status 1 | stays: never a success; display only |
| `serve/unit/script.rs:593` | rule | `unit_of(..).ok()` | stays: the diagnostic's unit, display only |
| `serve/unit/script/global.rs:49` | rule | a run key that isn't public is run locally, not fetched | stays: `blaze_global::Rule` decides up front; a refused fetch is already an error |
| `serve/unit/test.rs:425` | error | a test artifact with no executable was skipped: its tests silently didn't run | `UnitError::NoTestExecutable` when it is a test unit's. Done |
| `serve/unit/test.rs:608` | error | a trace that can't be told was kept with no trace (a weaker record) and the run still passed | the run fails, the suite's selector is not given back, nothing is kept. Done |
| `serve/unit/test.rs:668` | error | a suite with no selector was skipped: its tests silently didn't run | `UnitError::NoSelector`, the run fails. Done |
| `serve/unit/test.rs:721` | error | a test directory name that isn't a file name ended the run as passed with no test run | `UnitError::TestDir`, the run fails. Done |
| `serve/unit/test.rs:441` | error | a manifest in no directory ran the tests in "" | `UnitError::ManifestDir`. Done |
| `serve/unit/test.rs:728` | rule | unknown parallelism runs one test at a time | stays: libtest's `get_concurrency` does the same |
| `serve/unit/test.rs:698` | rule | no expected duration for a test with no history | stays: genuinely empty the first time |
| `serve/unit/test.rs:67`, `:186` | rule | a target with no kind named `:name`; a `--ub` test unit's binary named by its target | stays for now: display names (`unit_target` refuses no kind); note for later |
| `serve/unit/run.rs:178` | dead → error | `executable.unwrap_or(path)`: `runnable` already requires an executable | `UnitError::NoExecutable`. Done |
| `serve/unit/run.rs:170` | error | the one package's `default-run`, its package not found, was ignored and "the only binary" chosen | an error naming the package. Done |
| `serve/unit/run.rs:199` | error | a manifest in no directory gave the sandbox a "" read path | `UnitError::ManifestDir`. Done |
| `serve/unit/run.rs:260` | error | a program end that is neither an exit nor a failure reported false with nothing said | `UnitError::RunEnded` in the `Ran`'s stderr. Done |
| `serve/unit/run.rs:56` `launch_of` | rule | release runs from a file, dev is an error until blaze runs in process | stays: kinds chosen up front by `blaze_lazy::allow`, no quiet start from a file |
| `serve/unit/reach.rs:66` | error | a unit whose package isn't found was left out of what a change reaches | the error. Done |
| `serve/unit/reach.rs:70` | rule | a unit with no crate (a run) has nothing to reach | stays |
| `serve/unit/deps.rs` (image refused, `refused`) | rule | a dependency image that wouldn't cover every input isn't stored, the crate resolves each build | stays: a cache must miss no input; the refusal is reported (`served`, `-v`) |
| `serve/unit/files.rs:111` | rule | a diagnostic's snippet with no readable source is left out | stays: display only |
| `serve/unit/artifact.rs:202` | rule | an output with no uplifted copy is named at its own path | stays: cargo's `bin_dst` |
| `serve/unit/artifact.rs:61` | rule | `DYLD_FALLBACK_LIBRARY_PATH` | stays: a variable's name |
| `serve/fetch.rs:140` | rule | a transfer retried per `net.retry` | stays: bounded (cargo's `Retry`), each failure's cause kept |
| `serve/fetch.rs:128` | rule | 404/410/451 from a sparse index is "no such file" | stays: cargo's `http_remote` |
| `serve/fetch/archive.rs:45` | rule | a `.cargo-ok` in a `.crate` is skipped | stays: cargo's unpack |
| `serve/fetch/git.rs:83` | rule | a scratch directory that can't be removed is warned of | stays: cleanup after the fetch is done, its result unaffected |
| `serve/fetch/git.rs:30` | rule | the short commit of an error's title | stays: display; the full id is checked at `:37` |
| `serve/unit/codegen.rs:830-862` (parent's) | error | the lead's rows: the kept image taken out before the link and not put back on `?`; an `Output` that differs drops it for a full link; link errors as strings | parent's (no-fallback-link) |
| `serve/unit/compile.rs:728` (parent's) | error | `cbor::to_vec(policy).unwrap_or_default()`: a policy that fails to encode keys every test context as empty bytes, a key missing its input | parent's: the encode error |
| `serve/unit/compile.rs:283` (parent's) | error | `Inputs::file(..).ok().flatten()`: a native input that fails to read is "absent", so a record check can pass or fail on an IO error | parent's: the read error |
| `serve/unit/compile.rs:289-291`, `:327-332` (parent's) | error | a record key error, store read error or record that doesn't decode is "not fresh" / "no record": a corrupt record rebuilds silently | parent's: a corrupt record is an error naming the key; a miss stays a miss |
| `serve/unit/compile.rs:128-141` (parent's) | error | a header cut short is "not kept" | parent's: corrupt vs changed told apart |
| `serve/unit/codegen.rs:371` (parent's) | rule | a profile put not found isn't given back | parent's to judge |
