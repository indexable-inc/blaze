---
status: plan
summary: no-fallback's link area (2026-10-30): every fallback on the edit-to-running path of blaze-link, blaze-live, blaze-stencil, blaze-cargo, blaze-build-script, blaze-engine and harness/bench.py, classified (error / rule / dead) with what each becomes; each crate's own table is under link/.
date: 2026-10-30
---
# no-fallback: link

The lane's rule and kinds are in [../no-fallback.md](../no-fallback.md). This table holds the
link path in blaze-cargo (`serve/unit/codegen.rs`, `serve/unit/compile.rs`, `serve/job.rs`,
`serve/work.rs`) and the bench. Each other crate's table is its own file:

| crates | table |
|---|---|
| blaze-link | [link/native.md](link/native.md) |
| blaze-live, blaze-stencil | [link/live.md](link/live.md) |
| blaze-cargo, but the files below and the sandbox | [link/cargo.md](link/cargo.md), [link/cargo-cli.md](link/cargo-cli.md), [link/cargo-units.md](link/cargo-units.md) |
| blaze-build-script, blaze-engine | [link/scripts.md](link/scripts.md) |

## The link path, ranked

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `crates/blaze-cargo/src/serve/unit/codegen.rs` `link` (was :725-745) | error | the kept image is `swap_remove`d before the link; every `?` after it returned without putting it back, so the next build had no image and linked in full ("no executable to add to") | done (speed, 01157, then here): the image goes back on every path. A new image (no image kept, or one of another output) is made before the kept one is let go (`kept_image`), so an error there leaves `images` as it was. A refused `Image::stage` and a refused `Image::bytes` leave the image unchanged (blaze-link: the stage works on a clone of `State`, only interning symbols and caching parsed natives; `bytes` fails only before it takes the new state), so the next link stages against it |
| `codegen.rs` `link` (was :726, `kept.filter(\|it\| it.output == output)`) | error | a kept image whose `Output` differs was dropped and a new one made: a full link with no reason given | done: `OutputFields::between` names each field that differs (an exhaustive destructure, so a new `Output` field can't be missed), and the link is `Linking::Full(Full::Output(fields))`, said as `not spliced: the output changed: <fields>`. Why not key images by (path, output): a shared library's `exports` are every name it defines, so an edit that adds a function changes its `Output`; keying by output would call that relink a first link and hide it. Why not refuse the build: a changed profile (debug info, sanitizers) can't be patched by definition, and refusing it would break builds rustc makes (superset-of-rust). So the link is made, says why it is whole, and the bench fails that step. The real fix for `exports` is blaze-link staging a change of exports (link/native.md) |
| `codegen.rs` `link`, `relink` (was :716-817, `map_err(\|it\| it.to_string())`) | error | the linker's errors became strings | done: `LinkFailed { image, cause: LinkCause }`, `LinkCause::Linker { step: LinkStep, error: blaze_link::LinkError }` and typed `Native`, `NativeGone`, `NoToolchain`, `Unnamed`; errors from the steps that give strings today (build-script `-l`s, side tables, the test table) are `LinkCause::Given(String)`, to type with those steps. Reported where the unit ends: `compile.rs` turns it into the unit's refused diagnostic |
| `codegen.rs` `link` (01156) | error | v2 said nothing about how it linked, so the bench could not tell a splice from a full link | done: `link` gives `LinkMade { how: Linking }` (`First`, `Spliced`, `Full(Full)`) from `Staged::full()`, carried by `Compiled::links` to the loop, then `blaze_api::Worked::links` (`Linked { how: LinkHow, path }`, schema 16). The client prints one `link: <path> (<how>)` line an image to stderr in every format (`blaze_ui::api::link_lines`, from `cli/print.rs`), and `work_json` has `links` (01163, taken from ci) |
| `crates/blaze-cargo/src/serve/job.rs` `worked` (was :2298) | error | `Worked::link` was always `LinkKind::Placed`, a value guessed for one the job didn't keep | done: `link_kind(&job.links)`, placed when any link was not spliced, else patched |
| `job.rs` `Step::run`, `Does::Compile` | error | a compile that gave way (`Failed::Again`: a cycle of waits, memory) dropped the images it was given, and its next try linked them in full | done: it puts them in `Paths::back`, which `Paths::take_images` drains into the kept images before any is taken |
| `job.rs` `Step::run` on a panicking compile | error (reported) | a compile that panics drops the images it holds | stays: the panic fails its unit with its message and where it was raised, as an error of that unit, and the build goes on as after any failed unit; the next link of that path is a first link, which the bench fails as "linked before and its image was not kept" |
| `crates/blaze-cargo/src/serve/work.rs` `Image` doc | dead | said an image not given back "is linked whole next time" | rewritten to the rule above |
| `crates/blaze-cargo/src/serve/unit/compile.rs` `make`, `made.context` | error | a test binary's suite context had no native input in it, so tests whose binary relinked against a changed C library were selected as unchanged: an older result reused | done: `with_natives` folds the content of each native input the link read into the context |
| `harness/bench.py` `link_kind` (was :551) | error | printed why a step linked in full and timed it as a result | done: `link_kind(text, linked)` gives `spliced`, `first link` or `full link: <why>` (a first link of an image the daemon linked before is a full link); `edit_link` raises the harness's `Failed` for an edit or its taking back that was not spliced, or that said no `link:` line at all (a missing value, not a pass), before the step is timed, so the project's result says `failed` with the reason. Doctest in `link_kind`'s docstring |
| `harness/v2/engine.py:576`, `harness/v2/oracle.py:441` | error | the oracle records a non-spliced link as a number and judges the step by the rest | queued 01164 (coordinator): a step after `start` whose link is a full link fails |
| `codegen.rs` `relink`, test table `beside: ContentId::default()` | rule | zero is the exec table's "no premapped dependency image" (`blaze_codegen::exec::ExecTable::beside`) | stays |
| blaze-link `place.rs` "has room for N: link the image from scratch" | error | refused as it should be, but nothing can link the image from scratch: every later link fails the same way | typed (link/native.md: `LinkError::refusal`, `Refusal::NoRoom` and the rest), and it reaches the job typed inside `LinkCause::Linker`. The image is left as it was, so the refusal repeats until the edit that needed the room is taken back. Plan: the room is blaze-link's to provide, by sizing each region from the code it holds at layout and growing it in a stage as a planned step that `Staged` reports (so `Linking` says it and the bench can tell it from a splice). Never a relayout taken because a stage was refused |

## Later

- `LinkCause::Given(String)`: type the errors of `natives`, `given`, `link_lib`, `link_arg`,
  `Generated::side` and `test::image::table` as their own variants.
- `blaze_link::LinkError { function, what: String }` stays a struct; link/native.md adds typed
  forms for the refusals the caller matches on.
