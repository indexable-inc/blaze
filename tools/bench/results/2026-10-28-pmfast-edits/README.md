# Proc macros on the edit path: what an edit reruns (lane pmfast, round 3)

ix VM, arm64, 16 vCPU, 32 GB, the flake's nightly. The corpus is the one of
`2026-10-27-pmfast` (serde, clap, tokio, thiserror, async-trait, pin-project, derive_more,
strum in one binary), plus a plain `helper()` function and `asker`, a path-dependency derive
that asks where its item is (`Span::line`, `column`, `source_text`) and prints it.

Each run starts the daemon as `blaze build` does (`blaze <ws> --own-target --crate
pmcorpus --out .. --exe .. --watch`, `BLAZE_STATS=1`, an empty store), waits for the cold
build, then makes each one-line edit four times (on, off, on, off) and reads the daemon's
`edit-to-binary` and its proc-macro counters (`proc macros:` for native, `wasm proc macros:`
for wasm). Then it starts the daemon again on the warm store (restart). cargo is `cargo build`
(debug) of the same workspace with the same edits. Medians of 4.

Edits: **body**, a literal in `helper`; **signature**, `helper`'s return type; **derive**,
`#[serde(default)]` on a field of a `Serialize, Deserialize` struct; **derive doc**, a doc
comment on a field of it; **top line**, a blank line inserted at the top of `main.rs`;
**tokio body**, a literal in `#[tokio::main] async fn main`.

## Edit-to-binary, ms, and the expansions each edit ran

main 92d3d96936 with the counters of this branch; wasm and native in turn.

| edit | wasm | native | cargo | wasm macros ran | native macros ran |
|---|---|---|---|---|---|
| body | 369 | 373 | 146 | 0 | 0 |
| signature | 364 | 362 | 181 | 0 | 0 |
| derive | 382 | 390 | 233 | 2 in 7 ms (off again: 2 found) | 2 in 10-20 ms (off again: 2 found) |
| derive doc | 368 | 378 | 228 | 2 in 7 ms | 2 in 10-20 ms |
| top line | 369 | 360 | 229 | 0 | 0 |
| tokio body | 366 | 369 | 180 | 1 in 1 ms | 1 |

Cold: wasm 40.0 s, native 41.3 s, cargo 3.4 s. Restart on the warm store: 4.09 / 4.06 s.

Only the edited item's macros run again: rust-analyzer's database keeps every expansion whose
input did not change, and spans are relative to their item, so a line inserted above an item
changes nothing it is given. Macro work is at most 7 ms of an edit (under 2%), and wasm's is
less than native's. What an edit costs is outside macros: per edit, define about 60 ms,
object 80 ms, the rest of compile 130 ms, link 80-100 ms (`timeline-us`), with the whole
crate's 1026 functions put in the object again. That is why blaze is behind cargo on every edit
here; it is lane perf's (edit path).

## This branch: a kept expansion loads no module; asked expansions kept with their answers

| | main | this branch |
|---|---|---|
| modules loaded at restart | 8, 11 ms | 0 |
| expansions run at restart | 0 (15 found) | 0 (15 found) |
| body / signature / derive / top line / tokio body, wasm, ms | 376 / 363 / 380 / 363 / 374 | 377 / 358 / 389 / 361 / 372 |

With `asker` in the corpus (its expansion asks the database, so main keeps none of them):

| edit | `asker`'s expansion, this branch | printed line right? |
|---|---|---|
| body, signature | found (the file changed, so the database asks again; the answers are the same) | yes, every edit |
| derive, derive doc | found | yes |
| top line | ran (its line changed), off again: found | yes |
| restart | found | yes |

main (wasm) prints the right line too, by running `asker` every time. Native prints line 1 and
no text: the native server speaks the protocol without questions (issue #823).
