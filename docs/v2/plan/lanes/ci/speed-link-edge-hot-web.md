---
status: plan
summary: blaze v2 ci lane, hot_lint pass on blaze-ui and blaze-web: allocation per record to the terminal and per record to every build page, before and after, the budget and differential tests that hold them, and the one data structure changed (a node's calls chained through one list).
date: 2026-10-29
---
# speed-link-edge-hot-web: allocation on the terminal's and the build page's hot paths

Part of [ci](../ci.md), after [speed-link-edge-ci-ui](speed-link-edge-ci-ui.md) and
[speed-link-edge-ci-web](speed-link-edge-ci-web.md). Paths: `crates/blaze-ui/` and
`crates/blaze-web/`. Nothing here was built or run (docs/v2/plan/goals/principles.md). The
counts below are read off the code, and the budget tests listed hold them once they run.

## The hot paths

- **A record to the terminal.** The daemon's records reach `Live::record_into`, which applies
  each to `Build` and redraws the region (the tree, the strip, the slowest functions under way),
  writing only the lines that changed. A cold build sends thousands of `Crate`, `Moved`,
  `Progress` and `Compiling` records a second.
- **A record to every page.** Until a page is asked for, `Kept::keep` holds each record of a
  workspace's last builds in deflate streams. After that, `World::apply` puts it in the Flecs
  world, the program tells it into its last build's records (`Program::tell`), and `State`
  appends it to the frame `pending` for every page, encoded once.

The edge is everything else: `blaze why` and the last lines (once per build or command), the
HTTP head and REST (once per request or connection), the world's setup, and error paths.

## State of the art

- **ratatui, indicatif:** draw into a kept buffer and diff it against the last frame. blaze-ui
  already did this (speed-link-edge-ci-ui). The gap was the model behind it: the `Compiling`
  list cloned two `String`s per function on every record, and the tree's critical path
  collected a fresh `Vec` on every draw.
- **Arrow/Polars-style adjacency, petgraph's `Csr`, and intrusive lists as in Linux's
  `list_head`:** one pool of edges for all nodes, not a `Vec` per node. The world's
  `out: Vec<Vec<u32>>` made one heap list per function with calls (150 000 for anim) and grew
  each.
- **A reused serializer (minicbor, ciborium over a kept `Vec`):** blaze-protocol's
  `cbor::Writer` keeps its scratch between values, but `cbor::append` makes a new one for each
  value.

## Before and after, per warm record

| path | before | after | test |
|---|---|---|---|
| `Live::record_into` + `tick_into` | 2 `String`s per function of each `Compiling`, 1 `Vec` per draw (`critical_path`) | 0: list entries rewritten in place, spare entries kept, the chain in `Scratch` | `a_warm_record_to_the_terminal_allocates_nothing` |
| `Kept::keep`, a build like the last | 1 `Vec` per Began (`said`), 4 growing buffers and a fresh output stream per run, a reallocation at `shrink_to_fit`, writer scratch per record | 0: the dropped runs and the encoder's other stream are kept (up to 2 runs, 1 MiB a buffer) | `a_build_kept_like_the_last_allocates_nothing` |
| `World::apply` of a warm `Functions` | each object's id map made anew per build, one `Vec` per new node, each grown on its calls | 0: the maps are cleared and kept, the calls are chained through one edge list that reuses freed edges | `a_build_told_every_page_like_the_last_allocates_nothing` |
| `Program::tell`, `State` pending | up to 2 (writer scratch) per record | 0, with a kept `cbor::Writer` each | as above |
| a frame flushed to N pages | 1 frame + N `Arc` counts | unchanged: one allocation for all pages | none |

Flecs allocates on its own C heap, which the counting allocator does not see. The budget tests
cover what Rust allocates.

## What changed

- **blaze-ui:**
  - The last lines and `blaze why`'s lines (`summary_line`, `why_lines`, `why_steps`, `tail`)
    are each written into one `String` through `Display` writers (`Dur`, `Short`, `Tail`,
    `Painted`), not built from pieces.
  - `Build::critical_path_into` fills a kept chain.
  - `Compiling` reuses its entries.
  - `Clone` is gone from `Strip`, `Stepped`, `Progress`, `Crated` and `Finished`.
  - `compiling_of`, `strip` and `plural` are now test-only. They are the scans and old forms
    the tests check against.
- **blaze-web:**
  - `world::Calls` replaces `Vec<Vec<u32>>`. Each node keeps a chain in one edge list, in the
    order its calls were made (the order of `Callees`), with a free list for the edges of a
    node said again.
  - Each object's id map is kept across builds.
  - `kept::Workspace` recycles its runs and the encoder's output.
  - `module_path` is test-only; placing uses `each_module`.
- Every remaining finding is justified in place: the edge, setup, error paths, and `Arc`
  clones, which only bump a count.

## Tests

- **Differential:** every rewritten blaze-ui line is compared with verbatim copies of the old
  functions in `tests/old.rs`, over 2000 seeded builds in color and without, and 300 for
  `why_steps`.
- **Model:** `Calls` is checked against a `Vec<Vec<u32>>` model, over 64 seeds of 3000 steps
  each.
- **Behavior held:**
  - `records_kept_in_runs_are_those_kept_one_by_one` holds the recycled runs to the old
    one-by-one `Kept`.
  - `calls_waiting_in_chains_are_those_a_scan_found` holds the world's calls to the old scan.
  - `ids_kept_from_build_to_build_are_each_builds_own` checks that a kept id map names nothing
    of the build before.
- **Budget:** the four `*allocates_nothing` tests above, each run after warm rounds.

## Lint (`/tmp/sle_lint.py`)

| crate | before | after |
|---|---|---|
| blaze-ui | 79 | 0 |
| blaze-web | 62 | 0 |

## Left

- The frame sent to the pages is one `Arc<[u8]>` per flush. Reusing it would need the pages to
  hand it back, and it already serves every page with one allocation.
- `module_calls` holds a `Vec` per module that calls another. It grows only when a new pair of
  modules is first seen, not per record.
