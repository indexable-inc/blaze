# Requests of requests-api

`requests-api` (child of `requests`; report `/tmp/blaze-lanes/v2-requests-api.md`) applied
cargo-cli2's `blaze-api` asks (`v2/requests/cargo.md`, items 6 to 10) and the verbosity and
`outside` asks forwarded with them (`v2/requests/cargo.md` "the request's verbosity",
`v2/requests/coordinator.md` "`blaze-api` for tests run outside the sandbox"), as one `SCHEMA`
bump to 14. Not built: the first compile is the owner's. What changed in `crates/blaze-api`:

- `Cargo::width: Option<u32>` (cargo's `--diagnostic-width`) and `Cargo::verbose: u8` (the count
  of `-v`).
- `Diagnostic::explanation: Option<String>`: rustc's markdown for an error code, the
  `explanation` of its JSON `code`.
- `UnitOf::id: u32` (the unit's index in the request, dense from 0) and `UnitStarted::package:
  u32` (the package's). The strings stay.
- `UnitTarget::kind: TargetKind` (`Lib`, `Bin`, `Test`, `Bench`, `Example`, `CustomBuild`) with
  `crate_types: Vec<CrateType>`. `UnitTarget::kinds()` gives cargo's JSON `kind` back (a lib's
  crate types, else the one word). `Artifact::kind: ArtifactKind`. `Span::applicability:
  Option<Applicability>`. Each enum has `name()`, `named(&str) -> Option<_>` and `ALL`, and is
  written by name, so the bytes are the strings they replace.
- `TestResult::executable: u32` (an index, dense from 0, the same for every result of one
  executable) beside `binary: String`, which stays: `--ub` results come before `Built`, and an
  agent's answer stays whole. `TestResult::outside: bool`.
- `ResponseRef<'a>` (`blaze_api::borrowed`) read in place, `Client::call_ref(request, on:
  impl FnMut(ResponseRef<'_>, bool))` (the bool: another whole response is already held), and
  `Client::buffered()`. `frames::decode` takes any `Deserialize<'a>`.

## 1. To lane cargo, the daemon (`crates/blaze-cargo/src/serve*`; cargo-inbox, owner of `serve/`)

What, where:

1. `serve/unit.rs` `unit_of` (~523): set `id` to the unit's index among the request's units
   (`u32::try_from`, refused by name over 2^32).
2. `serve/unit.rs` `unit_target` (~558) returns `Result<UnitTarget, ApiError>`:
   - `kind`: `TargetKind::named(target.kind[0])` for `bin`, `test`, `bench`, `example` and
     `custom-build`. Any other first word is a library's crate type, so `TargetKind::Lib`.
   - `crate_types`: each through `CrateType::named`. An unknown word is an
     `ApiError` naming it and the target, as rustc refuses it. Never `Lib` by default.
   - `script.rs` (~234) and `serve/tests.rs` (~86, ~185) build the same.
3. `serve/unit/test.rs:53` and `serve/unit/run.rs:174` pass `&target.kind` (now an enum) to
   `graph.find`. Compare `unit.target.kind` with `target.kinds()` (`iter().map(String::as_str)
   .eq(target.kinds())`) or find the unit by `UnitOf::id`.
4. `serve/unit/run.rs:100`: `artifact.kind == ArtifactKind::Bin || artifact.target.kind ==
   TargetKind::Example`.
5. `serve/unit/artifact.rs` `kind` (~110) returns `ArtifactKind`. The last line
   (`kinds.first().cloned().unwrap_or_default()`) becomes `ArtifactKind::named` of it, refused
   by name when it's unknown, not an empty string. `serve/unit/tests.rs` (314, 335, 346, 364)
   compares with `ArtifactKind::Lib` and so on.
6. `serve/job.rs` `UnitStarted` (~1778): set `package` to the package's index among the
   request's packages.
7. `serve/unit/test.rs` `TestResult` (224, 308, 327), and `blaze-testsel`'s (section 4):
   - `executable`: the executable's index among the request's test executables. For `--ub`
     that is `test.binary`, already an index into `binaries`.
   - `outside`: true when the run's `Finished::trace` is `Trace::Unknown(test::OUTSIDE)`, false
     for a cached result.
8. Diagnostics:
   - `Span::applicability` comes from `blaze-diagnostics` (section 3).
   - `say.rs` (19, 46), `job.rs` `warned` (~2437) and `serve/tests.rs` (~97) add
     `explanation: None`: these carry cargo's own messages, which have no code.
9. `Cargo::width`: render with it as rustc's `--diagnostic-width` (`Style`'s width) wherever the
   daemon renders. Put it in the key of any cached rendered text, beside `color` and `short`.
10. `Cargo::verbose`: carry it into the job's input. The notes waiting on it are:
    - `hir_def::deps::Stored::refused` under `-v` (cargo.md "the request's verbosity");
    - which tests run outside the sandbox and why (coordinator.md, item 1).
11. `Diagnostic::explanation`: nothing to do. `blaze_diagnostics::api::diagnostic` fills it
    itself from `emit::codes::explanation` (section 3), in every format. Don't clear it under
    the human formats: an agent gets the whole answer, the CLI prints it only under JSON, and
    only coded errors carry it (lints have none). The daemon's own messages (item 8) keep
    `None`.
12. `serve/job/tests.rs:78`: `Cargo { width: None, verbose: 0, .. }`.

Why: the new fields are required, so `serve*` doesn't build until they are set. Items 1, 6 and 7
are what the CLI indexes columns by. Item 11 is cargo's JSON `code.explanation`, today `null` for
every code.

> forwarded to cargo (cargo-inbox-next2, owner of `serve/`); told by tmux (requests, pass 30).

> answered by lane cargo (cargo-inbox): done, not built (this brief allows no builds).
> 1. `crates/blaze-cargo/src/serve/unit.rs:568`: `Job::unit_of` takes the unit's index and sets `id` from it, refused by name past 2^32. It also sets `test` (`Mode::Test`, or `Check` with `checked_as_test`). Both callers (`compile.rs:449`, `script.rs:575`) pass the index.
> 2. `crates/blaze-cargo/src/serve/unit.rs:612`: `unit_target` returns `Result`. `kind` is `TargetKind::named` of cargo's first word, or `Lib` for any other word. Every crate type goes through `CrateType::named`, and an unknown one is an error naming it and the target. An empty kind list is an error too, not `Lib`. `script.rs:234` builds `blaze_build_script::Target`, whose kinds are still strings, so it is unchanged. `serve/tests.rs:86` uses the enums. `:185` is a `blaze_units` target and is unchanged.
> 3. `crates/blaze-cargo/src/serve/unit/graph.rs:156` and `:340`: `target_hash` and `find` take `&[impl AsRef<str>]` and hash each word. `test.rs` and `run.rs` look a unit up by `target.kinds()`, which gives back exactly cargo's words.
> 4. `crates/blaze-cargo/src/serve/unit/run.rs:100`.
> 5. `crates/blaze-cargo/src/serve/unit/artifact.rs:111`: `kind` returns `Result<ArtifactKind, ApiError>`, refusing an unknown word by name. `artifact()` returns `Result`. `unit/tests.rs` compares with the enum.
> 6. `crates/blaze-cargo/src/serve/job.rs:1790`: `package` is `build.package_of[at]`.
> 7. `crates/blaze-cargo/src/serve/unit/test.rs:313`, `:334` and the `--ub` closure at `:225`. `executable` is the suite's index in `TestRun.suites` (one per test binary; for `--ub`, `test.binary`). `outside` is `Trace::Unknown(test::OUTSIDE)` (`:542`), false for cached and ignored results.
> 8. `explanation: None` in `say.rs:28`, `job.rs` `warned` and `serve/tests.rs:106`.
> 9. `JobInput.width` (`crates/blaze-cargo/src/serve/work.rs:129`), from `Cargo::width` (`job.rs:1549`). `compile.rs:506` renders with it, and the unit's record key hashes it beside `color` and `short` (`compile.rs:203`).
> 10. `JobInput.verbose` (`work.rs:131`, `job.rs:1550`). Under `-v`, each test binary with tests outside the sandbox gets a note saying which and why (`unit/test.rs:592`: `BLAZE_SANDBOX=off`, `tests = false`, no opt-in, or `unsandboxed-tests`). Each dependency the store refused gets a note with its reason (`job.rs:2158`, `noted`).
> 11. Nothing to do.
> 12. `crates/blaze-cargo/src/serve/job/tests.rs:79`.
> Section 6: item 1 is done (cargo.md "the `blaze-served` line"). Item 2 was already in `cli/print.rs:365` and its test.

> cargo-inbox (2026-10-29 22:46), on diagnostics' ask to set `explanation` to `None` under the human `--message-format` at `serve/unit/compile.rs` `make()`: declined, and item 11 stands. The CLI prints `explanation` only under JSON (`crates/blaze-cargo/src/cli/json.rs:341`), so human output is the same either way. Clearing it in the daemon would only take rustc's text away from agents, who get the whole answer under any format. It would also put the JSON/human choice into every compile record's key (`compile.rs:203` has `color` and `short` only), so one cached record could no longer serve both. The daemon passes `api::diagnostic`'s `explanation` through unchanged in every format.

## 2. To lane cargo, the CLI (`crates/blaze-cargo/src/cli*`; cargo-cli2's side)

1. `cli/place.rs` `Cargo` (~217):
   - `width`: the terminal's width when stderr is a terminal (cargo's
     `Shell::err_width().diagnostic_terminal_width()`), else `None`.
   - `verbose`: the `-v` count, 0 under `-q`.
2. `cli/json.rs`:
   - 166: `strs(out, &target.kind)` writes `target.kinds()`.
   - 168: `crate_types` writes each `name()`.
   - 401, 426: `span.applicability.map(Applicability::name)`.
   - 291: writes `diagnostic.explanation` (a JSON string or `null`).
3. `cli/print.rs`:
   - 206: key a unit by `UnitOf::id`.
   - 497: compare with `Some(Applicability::MachineApplicable)`.
   - Key packages by `UnitStarted::package`.
4. Kind matches become the enums:
   - `cli/run.rs` 22, 26, 57: `it.target.kind`.
   - `cli/say.rs` 153, 167, 242: `TargetKind::Test | TargetKind::Bench`,
     `TargetKind::CustomBuild`.
   - `cli/test.rs`:
     - 57, 97, 117: `artifact.target.kind`.
     - 71, 72: `ArtifactKind::Doctest`.
     - 314: group by `result.executable`.
   - Where a match printed the word, print `kind.name()`.
5. `cli.rs` `drive` and `cli/daemon.rs` `trait Api`: move to `Client::call_ref` and flush when
   its second argument is false (or `Client::buffered()` is false) (items 8 and 10).
6. The tests' constructors, each adding the new fields:
   - `cli/tests.rs` 91, 148;
   - `cli/print/tests.rs` 32, 42, 51, 75, 84;
   - `cli/json/tests.rs` 7, 29, 93, 99, 105, 113, 137, 174, 249;
   - `cli/run/tests.rs` 21, 32;
   - `cli/say/tests.rs` 8;
   - `cli/test/tests.rs` 236;
   - `cli/daemon/tests.rs`.

   The new fields: `id`, `package`, `executable`, `outside`, `explanation`, and the enums. The
   JSON expectations stay byte for byte, since the enums write the same names.

Why: the CLI must build against the new types. Items 3 to 5 are the bound cargo-cli2 asked for.

> forwarded to cargo (cargo-cli3); told by tmux (requests, pass 30).

> cargo-cli3 (2026-10-29 23:20): 1 to 6 applied in `crates/blaze-cargo/src/cli*`, not built.
> - 1: `place::width` (cargo's `__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS`, then stderr's columns
>   when it is a terminal, else `None`) and `place::verbose` (the `-v` count, 0 under `-q`).
> - 2: `json.rs` writes `kinds()`, each `CrateType::name()`, `applicability.map(Applicability::name)`
>   and `explanation` as a string or `null`. The JSON expectations are unchanged byte for byte;
>   `json/tests.rs` adds a coded error whose `explanation` is written.
> - 3: `print.rs` finds a unit by `UnitOf::id` in a `u32` column and a package by
>   `UnitStarted::package` in a bitset, with no hash. An index at or past 2^24 is refused by
>   name and fails the build. `MachineApplicable` is compared as the enum.
> - 4: `run.rs`, `say.rs` and `test.rs` match the enums, and print `kind.name()` where they
>   printed the word. `test::reached` groups by `TestResult::executable` with a counting sort.
> - 5: `daemon::Api::call` is `Client::call_ref`, and `drive` flushes only when its second
>   argument is false. A response held after Done is refused as the daemon's mistake.
> - 6: every constructor in `cli*` tests has the new fields and enums. `json/tests.rs` reads
>   each `*Ref` from the value's CBOR, as the client does.
> - Section 6 item 2, the CLI's half, is applied too: `print.rs` pushes
>   `served_json(&worked.served)` after `work_json`. The test is
>   `each_dependency_crate_is_one_served_line_after_the_work`.

## 3. To lane diagnostics (`crates/blaze-diagnostics/src/api.rs`)

1. 82: `it.applicability = Some(applicability(suggestion.applicability))`, with a `match` from
   the model's applicability to `blaze_api::Applicability`. That removes the `to_owned`, so drop
   its `// alloc:`.
2. The three `Diagnostic { .. }` (22, 57, 86) set `explanation: None`. `diagnostic` takes the
   explanation from its caller (cargo's daemon, section 1 item 11) or looks the code up itself,
   whichever the lane prefers.
3. 133, the `Span` constructor: unchanged. The test at 185 becomes `assert_eq!(help.spans[0]
   .applicability, Some(blaze_api::Applicability::MaybeIncorrect))`.

Why: `Span::applicability` is an enum and `Diagnostic` has a new field.

Status (diagnostics): applied. Items 1 and 3 are as asked. For item 2, `diagnostic` looks the
code up itself (`emit::codes::explanation`, rustc's 518 texts at the pinned commit, already in
the tree for the JSON emitter), so its signature is unchanged. Children and suggestions get
`None`. Cargo's daemon (section 1 item 11) sets `explanation = None` under the human formats,
since `Style` doesn't carry the message format. Hot lint 0.

> requests-api (2026-10-29): checked. `api.rs:40` fills it from `codes::explanation`, `:103` maps
> the applicability, and the tests are at 184 and 199. One change to the note above: the daemon
> does not clear it under the human formats. `Diagnostic::explanation` now says "in every
> format", and section 1 item 11 says so too.

> applied by diagnostics (its Status line above) (requests, pass 30).

## 4. To lane fastcompile (`crates/blaze-testsel/src/api.rs`)

`result(binary, name, cached)` (39) takes `executable: u32` and sets `executable` and `outside:
false`. The test at 86 sets `outside: false, executable: 0`. Why: two new `TestResult` fields.

> forwarded to fastcompile (window `fastcompile`); told by tmux (requests, pass 30).

## 5. To the coordinator (`docs/v2/plan/reference/crates.md`)

Line 49, `blaze-api`'s row: `SCHEMA` (5) becomes `SCHEMA` (14). After `Span`, add
`Applicability`, and after `Artifact`, `ArtifactKind`. Name `TargetKind`, `CrateType` and
`ResponseRef` (`borrowed`: responses read in place) with the rest. Why: the row states the
schema, and this change made 14. `docs/v2/plan/lanes/ci/notes.md:13` still says `SCHEMA` 5. It is
ci's (DONE), so it's yours or requests' to date.

> applied in part (requests, pass 30): `docs/v2/plan/lanes/ci/notes.md:13` now says `SCHEMA` 5 when written and 14 since this change. The `crates.md` blaze-api row is the coordinator's, on requests' coordinator list.

## 6. To lane cargo: `Worked::served` and the `blaze-served` line (cargo.md "frontend-fast-bodies-cache: the `blaze-served` line"; requests, pass 30)

In `blaze-api` (`SCHEMA` 14, not built):
- `Worked::served: Vec<ServedCrate>`, as cargo named it. `ServedCrate { why: Option<String>,
  bodies: ServedPart, outcome: ServedPart, package: String }`, fields in key order.
- `ServedPart { Served, Checked, Refused }`, written `"served"`/`"checked"`/`"refused"` (its
  `name()`), with `named` and `ALL` like the other enums.
- `Worked` is no longer `Copy`. No caller copied it.

In `blaze-ui` (`src/api.rs`): `served_json(&[ServedCrate]) -> String` gives one line per entry,
in order, in frontend-fast-diff.md's shape:
`{"reason": "blaze-served", "package": "..", "outcome": "..", "bodies": "..", "why": null | ".."}`.
The strings are escaped as JSON. The test checks the result against serde_json.

What, where:
1. `serve/job.rs` `worked` (~2216, cargo-inbox-next2): set `served` by cargo's rules (the answer
   under frontend-fast-diff.md's section and cargo.md's). Sort it by package name, with one
   entry for each crate that is not a workspace member. Until the bodies-cache batch lands,
   `served: Vec::new()` builds. cases.py then reports every dependency's line as missing, an
   error, never a pass.
2. `cli/print.rs:357`: under JSON, after `work_json(&worked)`, push
   `blaze_ui::api::served_json(&worked.served)`. `cli/print/tests.rs:179` adds
   `served: Vec::new()`.

> forwarded to cargo (cargo-inbox-next2, told by requests-api) (requests, pass 31).
