# Blaze Feature Requests & Strictness Log

A running log of missing features, compiler/tooling gaps, `bstd` ergonomics, and **stricter-by-default rules, types, or lints** discovered while building on blaze.

## When to append an entry

Append a concise row/section whenever something in blaze — language syntax/semantics, stricter compile-time or lint checking, `bstd` APIs, diagnostics, or agent/dev tooling — would have **prevented a bug, confusion, or workaround**.

**Bias toward strictness:** If non-strictness or a silent default let a mistake slip through (missing exhaustiveness, ignored unknown keys, weak type boundary, silent fallback, unclear sandbox block), request the **stricter** check/type/lint that would make that bug class unrepresentable or fail fast.

Format for new entries:
- `### YYYY-MM-DD — <Short title>`
- **Area**: crate(s) / subsystem / doc
- **Kind**: `language` | `strictness/lint` | `bstd` | `diagnostic` | `tooling`
- **Friction / Bug**: what went wrong or required a workaround
- **Request**: concrete end-state feature or stricter check

---

### 2026-11-26 — End-to-end `ConstSetPatTy` (`T is in CONST_SLICE`) + `core::reflect::FinitePat`

- **Area**: `crates/blaze-ty/src/pat_ty.rs`, `crates/blaze-typeck`, `crates/blaze-consteval`, `library/core/src/reflect.rs`, `docs/v2/plan/design/refined-values.md`
- **Kind**: `language` + `strictness/lint`
- **Friction / Bug**: `ConstSetPatTy` exists in `blaze-ty::pat_ty` as a type algebra (`SemVer is in VERSIONS`, `&str is in ["1.1.1", "1.2.2"]`), but `blaze-typeck`, `matchck`, and `blaze-consteval` do not yet wire it end-to-end, forcing stringly-typed validation or manual enums when a domain is a finite const set.
- **Request**: Wire `T is in CONST_SLICE` end-to-end across lowering, `blaze-consteval` member evaluation, `blaze-typeck` subtyping/`&uniq` write guards, and `matchck` flow-sensitive exhaustiveness narrowing with missing-member witnesses, plus a `core::reflect::FinitePat` reflection surface for generic deserialization and validation.

### 2026-11-26 — Reflection-driven `decode_strict` vs `decode_lossy` in `blaze-data`

- **Area**: `crates/blaze-data`, `crates/blaze-std` (`bstd::data`, `bstd::comptime`)
- **Kind**: `bstd` + `strictness/lint`
- **Friction / Bug**: `FromValue`/`ToValue` are hand-written today, and callers who need forward-compatible/lenient decoding lack a structured report of what was ignored or defaulted, risking silent data loss when opting out of strict mode (`fields_allowing_extra`).
- **Request**: Provide reflection-driven `decode_strict::<T>` (default, refuses any unknown key or missing required field with path) alongside explicit `decode_lossy::<T> -> Result<Decoded<T>, Error>` where `Decoded<T> { value: T, unknown: Vec<Path>, defaulted: Vec<Path> }` makes every ignored or defaulted field inspectable by type.

### 2026-11-26 — `blaze-ctx` hierarchical summary tree + `blaze-tab` output + `agents/` annoyance indexing

- **Area**: `crates/blaze-ctx`, `crates/blaze-tab`, `agents/kb/`, `agents/howto/`
- **Kind**: `tooling`
- **Friction / Bug**: Agents spend multiple search/read turns reconstructing crate boundaries, module maps, and prior pitfalls spread across 190+ crates and the `agents/kb/` tables, often missing relevant annoyances or burning context on verbose JSON/grep output.
- **Request**: Make `blaze-ctx` maintain an item-/crate-grain hierarchical summary tree emitted in compact `blaze-tab` format and automatically surface matching `agents/howto/` and `agents/kb/` rows for any queried crate or path.

### 2026-11-26 — Fail fast on sandbox-blocked shell execution

- **Area**: agent harness / `AGENTS.md` / build scripts & CLI runner
- **Kind**: `strictness/lint` + `tooling`
- **Friction / Bug**: When the outer execution sandbox blocks shell commands or tool calls, agents can waste turns retrying alternate commands, probing ports/files, or falling back to partial static guesses instead of stopping loudly.
- **Request**: Detect sandbox-denied tool/shell calls explicitly with a typed refusal and immediately stop to request full permissions from the user — zero retries, zero silent fallbacks (consistent with rule 3 `no-fallback`).

### 2026-11-26 — Lint flagging $O(N)$ linear-search loops over slices (`SortedSlice::get` / `HashMap`)

- **Area**: `crates/blaze-lint`, `crates/blaze-own` (`SortedSlice`)
- **Kind**: `strictness/lint` + `bstd`
- **Friction / Bug**: While self-hosting blaze (Waves 12–19), hot mid-end and codegen passes repeatedly suffered quadratic $O(N^2)$ blowups from linear scans over `Vec` / slice lookup tables (`for &(k, v) in slice { if k == needle { return Some(v); } }`) invoked inside per-instruction or per-block loops, stalling mega-function compilation behind 600s timeouts.
- **Request**: Add a `blaze-lint` check flagging linear key-lookup loops (`for &(k, v) in slice { if k == needle { ... } }` or `.iter().find(|(k, _)| *k == needle)`) inside functions or nested loops and suggesting `blaze_own::SortedSlice::get` (binary search) or `blaze_hash::HashMap` with a structured fix hint.

### 2026-11-26 — Enforce `MakeWatch` telemetry & scale-test coverage for mid-end / codegen passes

- **Area**: `crates/blaze-ladder/src/run/make_one.rs`, `crates/blaze-cli`, `crates/blaze-lint`, `crates/blaze-lir`
- **Kind**: `strictness/lint` + `diagnostic`
- **Friction / Bug**: Production `Maker` sites (`engine_export.rs`, `wasm64_compile.rs`) silently dropped slow-function reporting when a post-hoc `.watch(...)` setter was omitted or when `build_lir` bypassed `MakeWatch` timing, hiding multi-second mega-functions until manual PID sampling; meanwhile quadratic mid-end passes shipped without large-N scale regression tests.
- **Request**: Require watch/telemetry policies by construction (no optional post-hoc setters; lint against passing `None` for `MakeWatch` in non-test crates) and add a `blaze-lint` / test-harness policy flagging new mid-end/LIR/emit passes that lack a super-linear scale guard test (e.g. $N = 10^4$ blocks/values).

### 2026-11-26 — Persist `Typed` body records for `Toolchain` and `Dependency` crates

- **Area**: `crates/blaze-cli/src/engine_export.rs`, `crates/blaze-ladder/src/front2/program/lazy/typed.rs`, `crates/blaze-cli/src/body_types.rs`
- **Kind**: `tooling` + `strictness/lint`
- **Friction / Bug**: `ByKind` currently reads persisted `Typed` body records only for workspace member crates and falls back to `BodySource::Run` re-typechecking for `Toolchain` (`core`/`alloc`/`std`) and `Dependency` crates during `engine_export.rs` and `blaze run`/`test`, burning redundant CPU time on every export/codegen cold miss.
- **Request**: Persist item-grained `Typed` body records for `Toolchain` and `Dependency` crates (shared via R2 for pinned `std`/crates.io) so `engine_export.rs` reads all reachable bodies via `BodySource::Read` and eliminates `BodySource::Run` re-typechecking entirely.

### 2026-11-27 — A doc claim of "built" is checked against a caller

- **Area**: `scripts/src/bin/docs.rs`, `docs/`
- **Kind**: `tooling` + `strictness/lint`
- **Friction / Bug**: A docs audit found about forty `current` or `plan` docs calling a design "built" whose crate was deleted or whose only callers are tests (`blaze-stage`, `blaze-malloc`, `blaze-test-reach`, `blaze-store-check`, `blaze-world-api`, `blaze-format-tiers`, `blaze-rel`, `blaze-profile` from the `blaze` binary, blaze-http's `h3` wire, `blaze_run::{heat, registry, tier}`); the docs check holds paths, not reachability.
- **Request**: A gate check that fails for a workspace crate no binary reaches (with a dated allowlist), and for a `current` doc naming such a crate without the words "built, not wired"; delete or wire each crate on the list.

### 2026-11-27 — Code that contradicts a stated rule, found while auditing docs

- **Area**: `crates/blaze-build/src/walk/tail_call.rs`, `crates/blaze-proc-macro/src/native_bridge.rs`, `crates/blaze-proc-macro/src/module.rs`, `crates/blaze-graph/src/sched/fiber.rs`, `crates/blaze-typeck`, `flake.nix`
- **Kind**: `strictness/lint`
- **Friction / Bug**: (1) `tail_call.rs:5` falls back to a call and a return for an ineligible `become` (rule 3; backend-next.md point 9 says refuse by name). (2) `native_bridge.rs:85-106` finds a proc-macro server through `rustc --print sysroot` (rules 1 and 3). (3) Compiled proc-macro and build-script modules are keyed per crate by SHA-256 (caches.md `compiled-modules`), not per item on `blaze_ids`. (4) Fiber code remains although D157/D161 decide no fibers. (5) blaze-typeck restarts from the top on `Need`; D161 decides park and resume. (6) `flake.nix` calls `.github/scripts/build_release.py`, which is not in the tree; `site/install.sh` and `harness/smoke/smoke.json` name a `blaze-engine` binary no crate builds. (7) Twelve code comments name the store folder `f5s3`; it is `f6s3`.
- **Request**: Fix each at its cause; for (1) and (2) a lint that refuses a second route (`rustcsites` already counts (2)'s kind and should fail on it).

### 2026-11-27 — Deleting a doc that code cites

- **Area**: `scripts/src/bin/docs.rs`
- **Kind**: `tooling`
- **Friction / Bug**: 154 comments and `clippy.toml` reasons cited one design doc by path and section title, so merging that doc meant editing about 290 files; cited section titles and decision ids are not checked at all, so a comment can cite a heading that was cut.
- **Request**: `docs check` also resolves a cited heading or `D<n>` id after a doc path in code comments, and code cites a decision id (stable) in preference to a doc section.
