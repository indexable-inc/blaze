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

- **Area**: `blaze-ctx` (planned), `crates/blaze-tab`, `agents/annoyances/`, `agents/howto/`
- **Kind**: `tooling`
- **Friction / Bug**: Agents spend multiple search/read turns reconstructing crate boundaries, module maps, and prior pitfalls spread across 190+ crates and 580+ `agents/annoyances/*.json` files, often missing relevant annoyances or burning context on verbose JSON/grep output.
- **Request**: Ship `blaze-ctx` to maintain an item-/crate-grain hierarchical summary tree emitted in compact `blaze-tab` format and automatically surface matching `agents/howto/` and `agents/annoyances/` rows for any queried crate or path.

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
