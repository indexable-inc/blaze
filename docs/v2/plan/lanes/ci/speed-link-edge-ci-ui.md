---
status: plan
summary: blaze-ui's terminal view brought up to the speed bar - a build model indexed instead of scanned, a live region drawn into one reused buffer and written as the lines that changed, and every line of plain and API output written into one buffer, with differential tests against the old renderer.
date: 2026-10-29
---
# speed-link-edge-ci-ui: the terminal's view of a build

`crates/blaze-ui/` turns the daemon's records into what a terminal shows: `Build::apply` keeps
the model, `Live` draws the region at the bottom (interactive), `Plain` says each line once
(piped), and `api.rs` turns `blaze_api::Response`s into lines. The hot path is a cold build:
thousands of `Crate`, `Node`, `Moved`, `Started`, `Progress` and `Ended` records, each of which
used to cost a redraw of the whole region.

## Benchmarks

- **ratatui's double buffer.** Two buffers, the frame drawn into the back one, diffed cell by
  cell against the front, and only changed cells written; then they swap. Matched at line
  granularity: `Live` keeps the region it showed (`shown`) and the one it draws (`next`), both
  `Frame { text, ends }`, writes only the lines that differ, and swaps them. A line is the unit
  because the region's lines are short and mostly change whole (a time, a count), and a line
  rewrite (`ESC[2K` and the line) needs no cursor column bookkeeping.
- **notcurses' damage tracking.** Rows not damaged since the last render are skipped, and the
  renderer picks the cheaper of moving the cursor or rewriting. Matched: each draw weighs the
  per-line form (the changed lines with `ESC[nA`/`ESC[nB` moves) against the tail form (up to
  the first changed line, `ESC[J`, the lines from there), and writes the shorter. So no draw is
  ever longer than the old clear-and-redraw.
- **indicatif's draw-target throttling.** Draws are rate-limited (20 Hz by default) and
  intermediate states are dropped. Not adopted as such: it changes which screens a user sees,
  and the region's intermediate screens are the test oracle. What is adopted is the batching
  part: `Live::records_into` takes every record of one socket read, says their staying lines
  in order, and draws the region once. The screen it leaves is exactly the one per-record
  drawing leaves. The caller's side is a request to lane cargo (below).
- **cargo's progress bar.** Redraws at most every 100 ms and only when the line changes, one
  line. `Live` draws nothing when the region is unchanged (a tick inside the same tenth of a
  second writes zero bytes), and keeps a multi-line region where cargo keeps one line.

## Before (per record, read from the code)

- `Build::apply`: `row(name)` and `node(id)` were linear scans over every crate and graph node,
  so a cold build's `Node`/`Moved` records were O(n) each and the build O(n^2). `Progress` and
  `Ended` scanned every step; `Said` cloned its text into the `Change`. Each `Node` record
  allocated its row name and its `deps: Vec<usize>`.
- `Live::record`: `staying` built a `String` per line, `region` a `Vec<String>` of lines, each
  line `format!`ed, `took` allocating once per step shown in the strip and once per crate
  under way. The tree pass was O(n + e) over the whole graph, plus an O(n log n) sort of rows,
  every draw. Then the whole region was rewritten: `\r ESC[nA ESC[J` and every line, about
  the region's size (300 to 1500 bytes) per record, even when nothing on it changed.
- `Plain` and `api.rs`: a `format!` per line, joined.

Allocations per record were in the tens (one per region line plus one per `took`), and bytes
per record were the region's size.

## Change

- `Build` keeps an `Index`: rows by name in a `hashbrown::HashTable<u32>` keyed by FxHash of
  the name (stored once, in its row), nodes by id in an `FxHashMap<u32, u32>`, the open steps,
  per-kind strip totals (`closed_ns`, open count, count, step), the fns totals and unknown
  count, a count per crate state, and the ended count. Under way is incremental: when a row
  becomes busy or idle, a worklist flips its nodes and their dependents over intrusive lists
  (`first_dependent`/`dependents`, `first_node`/`next_node`); the change is monotone so each
  node flips at most once. Graph deps are one CSR `Vec<u32>` (`Noded` is 20 bytes, `Copy`).
- `Change<'a>` borrows the record's text (`Said(Level, &'a str)`).
- Every line goes through `put!` (`write_fmt` into the caller's `String`) and Display writers
  (`Dur` for `took`, `Count`, `Painted`) that know their width in chars, so fitting a line to
  the terminal counts instead of formatting twice.
- `Live` draws into the reused `next` frame, diffs against `shown`, swaps. A resize marks the
  screen stale and the next draw is the old whole form; so is a draw after staying lines (they
  scroll the region away). `record`/`tick` keep their `-> String` signatures over new
  `record_into`/`tick_into`/`records_into`. The under-way list keeps the top `SHOWN` crates in
  an insertion array, the tree visits with an 8-byte stack entry from lazily found roots, and
  its hidden count is `busy_graphed - shown`. The caps `SHOWN` and `TREE` are unchanged.
- `Plain::record_into`/`tick_into` and `api::lines_into` write into one buffer; `lines`
  wraps it.

## After (per record, read from the code)

- `apply` is O(1) per record, plus O(flipped nodes) for a state change; zero allocations after
  warm-up except what the build keeps (a new crate's name, `of`, rebuilt and refused names).
- A draw allocates nothing after warm-up (the frames, the strip rows, the tree stack and the
  deps scratch are reused). The tree costs an O(n) scan for roots and O(shown) after it.
- Bytes per record are the changed lines plus their moves: about one line (40 to 120 bytes)
  for a typical record, zero when the region is unchanged, never more than before.
- With batching, a 64 KiB read of hundreds of records is one draw and one write.

## Proof

`crates/blaze-ui/src/tests/old.rs` is the old renderer and `Build`, verbatim. `tests.rs` has a
model terminal (`\n`, `\r`, `ESC[nA`, `ESC[nB`, `ESC[J`, `ESC[K`, `ESC[2K`, SGR) and a seeded
record generator, and checks over 300 seeds that the new output fed to the terminal shows
exactly what the old did after every record, tick, resize and finish, that no draw is longer,
that the new `Build` agrees with the old one field by field and its indexes with linear scans,
that `records_into` leaves the screen per-record drawing leaves, and that `Plain` writes the
old bytes exactly.

## Requests

- lane cargo (`v2/requests/cargo.md`): `client.rs` `exchange` and `say.rs` `Shown` collect one
  read's records and call `Live::records_into` once, with one write.
