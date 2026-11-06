---
status: current
summary: The flight recorder names strings per thread. Each thread interns into a private table whose ids are scoped to its own ring, defines each name in the ring the first time it uses it, and starts a new generation at chunk boundaries. Readers turn per-thread ids into global ones as they decode, so the hot path shares nothing and the profile parquet is unchanged.
date: 2026-11-05
---
# Per-thread names in the flight recorder

## Why

`blaze_profile::flight::intern` turned every span-field value, event message and crate or item
name into a `Sym` through one process-wide table. Until #1805 it was 16 mutex shards. A holder
that stalled (sampled, preempted, faulting) queued 60+ threads behind it, the daemon's turn loop
among them, and `blaze status` stopped answering. #1805 swapped the mutexes for papaya maps. That
removed the convoy, but every name still shares cache lines across cores, a miss costs a
contended `fetch_add`, two lock-free inserts and an `Arc<str>`, and the postmortem name logs
needed a handoff queue so a giver never waits on a log.

The rows themselves were already per thread: each thread owns a `Ring`, and only that thread
writes it. Only the naming was shared. This change makes the naming per thread too, modelled
on Perfetto's per-sequence interning (`interned_data`, `SEQ_INCREMENTAL_STATE_CLEARED`) and
Tracy's per-thread queues.

## Design

**Static names cost nothing.** Span names, field names and callsites are `&'static Metadata`,
and the callsite's site number is already its id (`flight::register`). `span::Name::kept(&'static
str)` (a job's kind, a lock's name) is a kept name: a process-wide number given once per text,
found after a thread's first use by the text's address in a thread-local cache. After that
first use it needs no shared state and no allocation.

**Each thread interns dynamic text itself.** `flight::names::Local` is a thread's private table:
text to index, for the ring the thread holds. A `Name` (and a span slot's `crate`, `function`,
`unit` and every name column) holds a 64-bit **token**:

- `KEPT | kept number`
- `ring << 44 | generation << 12 | index` (generation 32 bits, index 12 bits, from 1)

The first use of a text in a generation writes its **definition** into the thread's own ring: a
`NAME` row (the index, the length, 56 bytes of text) and as many `NAME_MORE` rows as the rest
of the text needs, up to 64 KiB (longer text is cut and counted). Nothing atomic or shared is
touched. A hit costs a hash and a thread-local lookup.

**Rows carry ids scoped to their ring.** A row's name word is a 32-bit row sym:

- `KEPT_ROW | kept number`
- `generation & 0xff << 12 | index`

It always names a definition in the same ring, in a generation at most `WINDOW` (32) before the
ring's generation at that row. A token from another ring (a span opened on thread A and closed on
thread B, a column said by a fiber that moved, a `Name` passed between threads), or one older
than the window, is written as a `LINK` row. That row binds a fresh index of this ring's
generation to the full token. Readers resolve a link through the other ring's definitions by
full generation, so an id never aliases another string. When the reader no longer holds the
target generation, the name is counted as forgotten, never read as another.

**Generations reset at chunk boundaries, marked in the stream.** A thread starts a new
generation, and writes a `GEN` row with the full generation number, when it claims a ring, when
its generation has used all 4095 indexes, or once `GEN_ROWS` (4096) rows have been written since
the generation began. The ring keeps the next generation number, so a ring passed to a new
thread never reuses one. A row's own ids reach at most `WINDOW` generations back, and a reader
resolves them against the generation that covers the row's position (the last `GEN` row before
it), so the low byte names exactly one generation of those 32. A generation whose `GEN` row the
reader missed covers nothing, and its ids read as forgotten. Nothing is evicted while recording:
a table only grows within its generation and is dropped whole at the boundary.

**Buffer policy: the newest row wins, the writer never waits.** The ring is the flight
recorder's: a full ring writes over its oldest row. A reader that fell behind counts what it
missed (`Read::skipped`). A name whose definition was written over before any reader saw it reads
as none and is counted (`Read::names_forgotten`). The other policy, dropping the new row, was
rejected: the flight recorder exists to keep the last moments before a stall or a crash
(`flight::dump`, the postmortem), and those are the newest rows.

**Fibers migrate; rows don't care.** Buffers are per OS thread. A row already carries its job,
fiber and ticks (`switched`, `Slot::thread`), and a span's row says the ring that made it
(`ring_of(id)`), so a span opened on A and closed on B is written by B with B's ids. Its
A-scoped names go in as links. Every record looks up its thread's ring and table fresh
(`flight::ring()`, `names::with_local`, both `#[inline(never)]` and ending before any wait).
Nothing tied to a thread is held across a park (`no-wait-across-thread-locals`).

**The reader remaps.** `names::Streams` is the readers' state: per ring, the starts of the last
`RETAIN` (64) generations and each generation's definitions. It sits behind one lock that only
readers take (the history's drain, a window or dump, `threads_now`, `flight::name`). `decode`
turns each row sym into a process-global `Sym` from a reader-side table (two generations of 16 Mi
names, as before), so `SpanRow`, `EventRow` and `FieldRow` keep their `Sym` fields. Every reader
of them (`batches.rs`, `logs.rs`, `time.rs`, `hot/read.rs`) is unchanged, and the profile parquet
is byte for byte what it was. No schema bump. The explorer's reader range and #1769's release
gate are untouched.

**The core is sans-IO.** `flight/names.rs` is marked `#![blaze::sans_io]`. `Local` writes rows
through the driver's `Out` (`intern`, and `row_syms`, which turns all of one row's tokens into its
syms in one call, so no generation begins between two of them), and `Streams` takes rows in
through a `Source`, which says the first index it read and the head, so a reader knows which rows
it missed. The driver is `flight.rs` (`Ring::put`, `take`).

**The postmortem reads names from the rows.** A dead process's definitions are in its mapped
rings, so the 16 shard name logs, their handoff queue and `handed_off` go. One log remains, for
kept names, written under the kept table's lock, which is a cold path. The mapping's `LAYOUT`
goes to 4, so an older file is refused, never misread.

## Removed

`flight::Names`, `Shared`, `Shard`, `INTERN_TRIES`, `NameCounts::{raced, handed_off}`,
`postmortem::{NameLog, handed_off, HANDED_OFF}` and the `ProfileNamesRaced` and
`ProfileNamesHandedOff` plots. `ProfileNamesLinked` counts links; `NameCounts` is now `given`,
`linked` and `lost`. `.clippy.toml` bans the removed `flight::Names`, `flight::Shared` and
`postmortem::handed_off`, each pointing at `flight::names` (a clippy ban cannot be scoped to one
module, so papaya is not banned in the recorder by path).

## Cost budget

- A hit: one hash of the text and one thread-local table lookup, with no atomic and no allocation.
- A miss: one boxed copy of the text in the thread's table, and ⌈len / 56⌉ ring rows.
- A link: one row, once per foreign token per generation.
- Reader: one lock per read. Memory is bounded by `RETAIN` generations per ring.

## Proof

- **DST** (`flight/names/tests.rs`, on `blaze_core::sim::Sim`'s seeded `Rng`). Hegel draws
  threads, fibers that park and resume on other threads, texts, records, a thread that stalls
  while others fill their rings past `CAP` (rings drawn small), drain crashes (the reader's state
  dropped) and restarts. The oracle: every row the drain reads resolves to exactly the text
  recorded, or is counted as forgotten or skipped, and nothing resolves to a different text.
  Planted bugs it must catch: a foreign token written as a local id (no link), a generation
  started with no `GEN` row, a reader that ignores the window and takes the newest matching
  generation, and a fiber that keeps its thread's table across a park.
- **Round trip**: spans and events with dynamic names recorded on several threads, one span
  closed on another thread, read and written as the spans and events parquet, read back as the
  same strings.
- **Bench** (`crates/blaze-bench/benches/flight_names.rs`): N threads opening spans with a
  `Debug` field, distinct and repeated values, before (#1805's papaya table) and after.

**Long spans keep their names.** A span may stay open for hundreds of generations of its ring
(a 70 s task on a busy daemon). Each read collects the tokens the open slots hold
(`Ring::held_tokens`), and `Streams::hold` keeps their texts until the read after no slot holds
them, so the link the closing row writes resolves (`names/tests.rs`,
`a_name_a_long_span_holds_reads_back_past_retain_generations`, which also shows the name is
forgotten without the hold).

**Named counters.** `flight::plot_named(name: &'static str, value)` writes a plot row of
`Plot::Named` carrying a kept name. It reads back as a `counters` row whose `name` is that text,
for counters no `Plot` names (memory owners).

## Trade-offs kept

- A live reader resolves a row's name with one hash of its text per name per row (no per-name
  cache in `LiveNamer`).
- Two `Name`s are equal when their tokens are, or failing that their texts: the same text given
  on two threads is two tokens.
- A token kept long (more than `RETAIN` generations of its ring) reads as forgotten.
