---
status: plan
summary: lane no-fallback (2026-10-30): every place where blaze, unable to do something, quietly takes another path (a full link or rebuild instead of a patch, a slower backend, an older cached result, a guessed default) becomes a typed error naming what and where. Language rules called "fallback" (integer, float, never-type and diverging fallback) stay.
date: 2026-10-30
---
# no-fallback

Andrew, 2026-10-30 16:01: "remove fallbacks, should just fail (so can hill climb easier)". The
rule is AGENTS.md rule 3. When blaze can't do something, it stops with a typed error whose
fields hold the reason and the place, never a string. A fallback that only logs or prints why is
still a fallback.

Motivating case: on 2026-10-30 the cargo-speed bench's generic edit took 33 s on Linux through
"full link: no executable to add to", and a signature edit took 76 s through "full link: the
whole object was written again". Both were v1's blaze-engine splicer (tag `v0.20261030.1`). In
v2, the same shape is in `blaze-cargo` `serve/unit/codegen.rs` `link`: a failed stage drops the
kept image, and an `Output` that differs from the kept one gets a new image. In both cases the
next build silently links in full. After this lane, an incremental step that can't patch is an
error naming the reason, and `harness/bench.py` reports that step FAILED with it.

Not fallbacks: the first link of a binary (nothing to patch, so a full link is the only path),
and Rust's own rules named "fallback" (integer and float literal fallback to `i32`/`f64`,
never-type and diverging fallback, intrinsic fallback bodies, weak-external and
`/ALTERNATENAME` symbol fallback in COFF, and anything else rustc or the platform linker does the
same way).

## Kinds

Every site is one of:

- **error**: a real fallback, which becomes a typed error variant. Callers that relied on it
  handle the error explicitly or pass it up.
- **rule**: a language or platform rule that rustc (or the system linker) applies the same way.
  It stays.
- **dead**: a path nothing reaches, or one left from v1 (rustc, cargo, the old splicer). It is
  deleted.

## Areas

Each area's table (file:line, kind, what it does, what it becomes) is its child's file:

| area | crates | child | table |
|---|---|---|---|
| link | blaze-link, blaze-live, blaze-stencil, blaze-engine, blaze-cargo, blaze-build-script, `harness/bench.py` | no-fallback-link | [no-fallback/link.md](no-fallback/link.md) |
| front | hir-def, hir-expand, hir, hir-ty, hir-ty-mir, hir-ty-codegen, hir-ty-matchck, rustc_abi | no-fallback-front | [no-fallback/front.md](no-fallback/front.md) |
| tools | blaze-diagnostics, blaze-fmt, blaze-lint-defs, and every other crate | no-fallback-tools | [no-fallback/tools.md](no-fallback/tools.md) |

The inventory started from the 299 lines of "fallback"/"fall back" in v2's crates
(`/tmp/fallbacks.txt`, 2026-10-30 16:03). Most fallbacks don't use the word, so each area also
reads these shapes: `unwrap_or`/`unwrap_or_default`/`unwrap_or_else` on a value the program
needs, `.ok()` dropping an error, `Err(_) =>` that continues, `if let Ok` with a silent else, a
retry with no bound, and a slow path taken when a fast one refuses.

## Where it stands (2026-10-30 20:30)

The area tables hold about 184 errors, 227 rules and 24 dead paths. Every error row's edit went
into the round checkpoints through `fd6947867d` or is in the tree for the next round, all
uncompiled until the coordinator's check. The speed case is whole end to end. blaze-cargo's
`link` keeps the image on every path and names a full link's reason (`Linking::Full`). It
reaches the client as `blaze_api::Worked::links` (SCHEMA 16), and blaze-ui and the CLI print
it as `link: <path> (<how>)`. `harness/bench.py` fails an edit that was not spliced, or that
printed no link line, before timing it. Codegen no longer hands a function LLVM refuses to
Cranelift (`CodegenError::LlvmRefused`). Visibility, the prelude and const and struct paths
stop with a typed error naming what and where instead of guessing (AGENTS.md rule 5: the error
is judged by whether it helps, not by matching rustc's text or code).

Open: request 01195 (the coordinator's blaze-wasm `project()` after the no-std-uses-core
fallback went), 01239 (format: pattern-type range ends), 01164 (harness/v2 engine.py and
oracle.py in the bench's shape), the `AdtSelfType` struct-path row in
[no-fallback/front/ty.md](no-fallback/front/ty.md), and a region out of room that leaves the
image unpatchable ([no-fallback/link/native.md](no-fallback/link/native.md)).

## Found by the lead (link area, handed to no-fallback-link)

| site | kind | what | becomes |
|---|---|---|---|
| `crates/blaze-cargo/src/serve/unit/codegen.rs:725-740` | error | the kept image is taken out of `images` before the link; any `?` after it returns without putting it back, so the next build has no image and links in full | the image goes back on failure (a failed stage leaves the image as it was); the error is typed |
| `crates/blaze-cargo/src/serve/unit/codegen.rs:726` | error | a kept image whose `Output` differs is dropped silently and a new one made: a full link with no reason given | a typed reason naming the fields that differ, reported as the link kind; an incremental step with it fails the bench |
| `crates/blaze-cargo/src/serve/unit/codegen.rs:716-817` | error | `map_err(\|it\| it.to_string())`: the link's errors become strings | a typed error carrying `blaze_link::LinkError` |
| `crates/hir-ty-codegen/backend.rs:391` (`Counts::fallbacks`, `fell_back`) | error | LLVM gives a function up and Cranelift compiles it instead, counted by reason | front's to classify (hir-ty-codegen) |
| `crates/hir-ty-codegen/mod.rs:172` `CodegenError::Unsupported` "fall back to rustc", `NoSymbol`, `Private` | dead | v1's rustc-beside-blaze variants | front's to classify |
| `harness/bench.py:551` `link_kind` | error | prints why a step linked in full and times the slow path | an incremental step that is not spliced is FAILED with the reason |
