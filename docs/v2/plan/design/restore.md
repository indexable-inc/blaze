---
status: plan
summary: blaze v2: the daemon restores from disk instantly: a mapped image with no deserializing, IDs stable across restarts, changed files known from the OS's journal, and work paged in on demand.
date: 2026-10-29
---
# Instant restore

Today a cold start takes 35 s. v2's target is that the first answer after a restart, or after
a reboot, takes about as long as the same answer from a warm daemon, plus a few milliseconds.

- **The state is one mapped image.** Results, interners, edges, fingerprints, the VFS tree
  and the plane's history live in the store's packs and the database image
  ([db.md](../../plan.md)), laid out to be used in place ([data.md](data.md)). Restoring is
  `mmap` and checking one header: nothing is deserialized, rebuilt or rehashed. Pages come in
  when a query touches them, so a restart costs only what the next request reads.
- **IDs are stable across restarts.** Interned symbols, item IDs and query keys are indexes
  into mapped tables, so a restored ID means what it meant before. Nothing is re-interned, and
  no table maps old IDs to new.
- **Integrity checks are lazy.** The header carries the image's hash. Each page group carries
  a CRC32C (in hardware) checked the first time it's mapped in. A torn or corrupt group is
  dropped and recomputed from its inputs, and the error names it (no-fallback: a recompute
  of a known-bad page, never a silent read of it).
- **Changed files come from the OS's journal**, not a rescan:
  - **macOS:** FSEvents with the last event ID saved in the image (`sinceWhen`).
  - **Windows:** the NTFS USN journal from the saved USN.
  - **Linux:** no journal survives a restart, so one `statx` sweep over the tracked paths on
    every core (io_uring's `STATX` is prepared serially on one thread, which is slower than the
    calls themselves), compared on inode, size, mtime, ctime and mode. That's within 50 ms for
    100k files, a few ms on 8 cores, then fanotify.

  Only files whose metadata moved are hashed.
- **The daemon starts on demand.** It is socket-activated (launchd, systemd), so the first
  CLI call or agent connection starts it with no wait for a separate start step. Warm process
  pools (test fork servers, macro runtimes) come back lazily, on first use.
- **Proof:** `proof`'s budgets time restart to first answer, after a clean shutdown and after
  `kill -9`, on the corpus's biggest workspace.

## Where a restart stands (2026-11-02, lane v2-warm-restart)

- **What a build computed is in the store once it answers.** As a head's front end is kept,
  one task (`store kept front end`, `crates/blaze-cargo/src/serve/job.rs` `Jobs::store_kept`)
  writes what its build left for the store: the dependency crates' def-map images, check
  outcomes and checked bodies, their constants, the goal cache and the item store
  (`deps::store`). The build is reported done only once those puts are durable, so a stop, a
  `kill -9` or a reboot after the answer loses none of it. A failed build's front end is
  kept and stored too.
  - Before, this was written only when the same daemon's next build of the head lowered into
    the front end, so a build followed by a restart left the store none of it.
  - A build of the head asked while that store runs waits for it and then lowers into the
    stored front end, rather than lowering anew beside it.
  - Test: `what_a_build_left_for_the_store_is_durable_once_it_answers_and_survives_a_crash`
    in `crates/blaze-cargo/src/serve/tests.rs`.
- **Each phase of a start is a span** (target `blaze`, under `daemon start`):
  - `read the state` (the journal, the image's map, both stores' opens);
  - `restore the VFS`;
  - `open the database` (an empty one: no engine state is restored);
  - `restore the plane's history`.

  The budgets are `harness/v2/budgets.py`'s `restart-first-answer-clean` and
  `restart-first-answer-kill9`.
- **Still not restored: the front end itself.** The design is the next section; what it
  replaces is the plan to put the front end's memos in the image as `Persist` values.

## A dependency's front end is records (2026-11-11, lane records-1111; D5, D21)

One change for three gaps: the daemon's memory, the first request after a restart, and a
machine that never built std. A crate that does not change while the workspace is edited (std,
a registry dependency) has its front end in the store as records, read in place through the
pool when asked; only the workspace's own crates are heap memos.

**Measured** (hello-world, Linux aarch64, 32 cores, cargo-built `e453ac295a`, fresh home,
cold build, five edits, idle; `measure.py` with its restart rows):

| | resident | of it |
|---|---|---|
| after a cold build and five edits | 5,143 MB | regions 1,660, heap live 1,426, freed and kept 2,041 |
| after a restart, a no-op and two edits | 6,081 MB | regions 1,645, heap live 2,080, freed and kept 2,334 |

The first no-op after a restart is 3.9 s (`lower workspace` 2.6 s, of it `maintain` 2.45 s:
F2, R8) and the first edit 9.3 s (`codegen walk describe` 5.4 s: R7). So an image served today
saves no memory: it is decoded into the same heap values, after the crate was resolved from
source anyway. And a value let go from the heap gives nothing back: letting go of every
durable row's unread value (tried, taken out) freed 110 MiB live and 0 MB resident, for 15 ms
a build's end. Only what is a whole array, a region or a pool page leaves; that is why the
values have to be records and not smaller heap.

**Keys.** A record is found by content alone: the crate's source and manifest content
hashes, its cfg and features, the *content* hashes of its dependencies' records of the kinds
it reads (never their keys, so a dependency that changes and yields equal records stops
there), and the code identity of the stage that makes the kind (name resolution, signatures,
bodies: each its own, from `blaze_codegen_code`, the same in a cargo-built blaze). No path, no
directory. `deps::key` is this but for two things to change: it hashes each dependency's key,
and one identity for the whole front end.

**Kinds**, a crate each, largest first, and where each is today:

| Kind | Held live now | Today |
|---|---|---|
| the rows' graph (edges 108, readers 106, nodes 105 MiB) | 320 MiB | readers: not held for a library's rows (built, below); edges: step 2 |
| def map, lang items, name resolution's state | about 470 MiB | a record (`nameres::frozen`), decoded to heap; the maintainer keeps state for a served crate's readers |
| syntax, item trees, macro expansions | about 550 MiB | heap only, parsed again after a restart |
| signatures | about 110 MiB | a record read in place an item (`deps::signatures`), then memoized on the heap |
| bodies, inference, MIR | let go at a build's end | records by body key (the item store) |
| impls and method resolution's tables | with nameres | none |

**The engine.** A crate with records is *sealed*: one input of kind `HIGH` a crate, whose
value is the content hash of its records. A query keyed in a sealed crate is answered by a
reader (`Persist::Ref` into a page the task pins), not by a run: no value on the heap, no
edges, and its node is read as that one leaf. Next after the readers below, the same for a
library row's forward edges: it verifies by its input summary alone, and a durable write that
meets the summary runs it again; then the reader functions, kind by kind.

**Built: a library's rows are not kept as readers** (`blaze_query::db::frontier`, "Durable
readers"). A row that read nothing below kind `MEDIUM` is left out of the reverse index when
the host packs it at a build's end, once two packs in a row saw no durable write between
them (twice as many after each shed a durable write ended); a durable write makes the index
again from the rows before its view's walk. Measured on the same script: the index 102.5 to
12.9 MiB live (25.5 million readers to 1.09 million; the query storage 849.6 to 778.0 MiB);
the daemon's resident size the same within the runs' spread (5,502 against 5,493 MB after
the rows), so it is live memory the next step can give back, not footprint yet. The pack
that sheds is 103 to 116 ms (40 ms whole), after the answer; no row of the edit loop moved
past the runs' spread (the table). Tests: the changed set's simulation with inputs of each kind
and packs among its steps, its control `the_oracle_catches_a_durable_write_over_an_index_left_shed`,
`a_pack_sheds_the_durable_readers_and_a_durable_write_makes_the_index_again`, and the core's
`a_shedding_pack_keeps_every_reader_it_was_not_told_to_shed` with its control.

| Row (median, ms) | base, two runs | with the shed |
|---|---|---|
| no-op build | 11.6, 12.6 | 11.5 |
| no-op run | 13.4, 14.0 | 15.1 |
| body edit + run (25th percentile) | 130.5, 138.8 | 133.9 |
| comment edit + run | 20.5, 38.4 | 24.3 |
| body edit after 3.5 s idle | 153.8, 155.5 | 162.1 |
| body edit after 8 s idle | 148.0, 180.0 | 163.4 |
| resident after the rows, MB | 5,493, 5,585 | 5,502 |
| first no-op after a restart, s (three runs each) | 3.9, 3.9, 4.1 | 5.1, 5.2, 4.4 |
| first edit after a restart, s | 9.3, 11.2, 9.5 | 9.4, 9.3, 9.7 |
| resident after the restart's builds, MB | 6,081, 5,975, 6,020 | 6,013, 5,898, 5,946 |

The restart's first no-op is not explained: its request is 3.38 s on the base and 3.48 s with
the shed, and the rest is before the daemon listens, where one `profile fsync` took 0.13 to
1.34 s on both binaries. Until that is off the start's path the row cannot be compared.


**Found: a library's rows are not at `HIGH`.** Of hello-world's 2,105,574 rows 181,865 are at
`HIGH`, 1,906,504 at `MEDIUM` and 17,205 at `LOW` (the pack's span says it): std's rows read
the maintainers' published tables (`Resolutions`, `ImplSlices`), the crate graph and their
files' source roots, all `MEDIUM`. So "a durable row is current with one compare" holds only
until any of those is written, and a sealed crate's leaf has to be an input of its own at
`HIGH`, apart from the workspace's tables. That is the first thing step 2 changes.

**The pool.** What is hot is resident; the rest is evicted under `Tag::Pool`'s budget, by
reload cost a byte, then recency, and the pages go back (`Store::evict_pool`, called by the
idle give-back and the ceiling guard; it has no caller yet). A miss is a page read. A record
that is missing or of an older layout is a named miss, `records of <crate>, <kind>, layout
<n>: made again from source`, never an empty answer.

**Restart** reads nothing eagerly. The lowering's batch asks the store before any crate is
resolved (F2) and writes no input after a result exists (the restart-then-edit build wrote
4,722 inputs over 551,000 rows, and ran 2.29 million queries); a describe is a record (R7);
the no-op's answer is kept (F3). **A new machine** fetches the same records by key from the
global cache (F4); nothing else differs.

**Order**, each step with before and after from `measure.py`, a round trip, and no edit-loop
row over 10% slower: (1) the graph's readers (built: above); (2) a sealed crate's leaf at
`HIGH`, then the graph's edges; (3) def
maps read in place and name resolution's state dropped for a sealed crate; (4) F2 and R8, then
R7; (5) `evict_pool` wired. Milestones: under 1 GB, then 500 MB; the first request after a
restart under 1 s, then 300 ms. The largest single part is none of the kinds: 2,041 MB freed
and kept by the allocator (transparent huge pages 1,134, sparse pages 293), which records
take down only by taking the churn off the heap.

## Memory: a bounded cache over records (2026-11-12, lane memstrat-1111; D21, D29)

The daemon stays. What it holds becomes a cache with a budget over records in the store; this
section is the order the records section above is built in, with what Relax does beside it.

**Measured, one machine** (18-core Mac, 2026-11-12; Relax `cea21cc` release build, blaze
`3a58d0cbae` cargo-built; medians, n = 9 a row for Relax and 20 to 40 for blaze, every run's
output and status checked; scripts in the lane's report). Relax's memory is the run's peak, since
no process outlives a run; blaze's is the idle daemon after a cold build and five edits.

| | Relax hello | blaze hello | Relax, six dependencies | blaze, six dependencies |
|---|---|---|---|---|
| cold | 0.69 s, 250 MB resident | 8.6 s | 4.4 s, 532 MB | 27.4 s |
| no-op run | 52.5 ms, 87 MB (footprint 50) | 11.6 ms | 60.4 ms, 130 MB (66) | 36.0 ms |
| comment edit + run | 90.2 ms, 117 MB (71) | 47.4 ms | 112.2 ms, 194 MB (113) | 102.2 ms |
| body edit + run | 101.3 ms, 127 MB (79) | 120.4 ms | 118.8 ms, 212 MB (126) | 255.6 ms |
| held between runs | nothing | 5,574 MB resident, footprint 3,641 | nothing | 7,212 MB, footprint 5,003 |
| on disk | 41 MB, 1.8 MB more an edit | 765 MB | 146 MB | 1,119 MB |

The 37.8 MB first quoted for Relax was a run that failed (`relax run hello` from the parent
directory finds no manifest and exits 1). The dependencies are regex, hashbrown, bitflags, itoa,
memchr and smallvec (Relax substitutes its own smallvec).

**How Relax does it** (its code, `libs/relax/`): a process a run over content-keyed store
files (`<target>/makepad-run/cache/*.rcs`), mapped read-only and shared
(`compiler/src/pack.rs:41-65`), never a daemon.

| Phase | Relax keeps on disk, and reads back | blaze today (hello-world, Linux) |
|---|---|---|
| lex, parse | a file's tokens, trees and positions as plain arrays, one image a file (`base/src/image.rs:1`, `compiler/src/front.rs:1`), read in place from the mapping: 0.0 MB of 14 MB on the heap; only files the program's names reach (`front/src/lazy.rs:1`): 174 files, 73k of its std's 134k lines | all of std parsed and expanded (24,700 item trees, 45,238 expansions run cold); heap `parse` 166 and `expand` 77 MiB held |
| names, definitions, the type table | the whole collected program as one image (`compiler/src/cache/collected.rs:1`: 25,560 definitions, 9.4 MB), behind one certificate of every input; a changed crate and every crate above it are collected again whole (`collected.rs:62`) | def maps and name resolution's state 215 MiB heap and 238 MiB regions; an image a dependency, decoded to the same heap values after the crate was resolved from source |
| type and borrow check | results not kept: a clean verdict a body owner (`cache.rs:61`), and a crate that an edit reaches is checked again whole (`cache.rs:96`, `:115`: one body edit in a path dependency checked both crates, 3 body owners); a dependency's body is checked when first called | 2.1 million query rows with their graph: 713 MiB of regions, 211 MiB heap |
| IR | not kept (an in-process memo of 64 MB at most, `bmemo.rs:43`) | let go at a build's end |
| machine code | a function's code with relocations by stable identity, keyed by its body and the closure hash of the items its names reach (`cache.rs:1`, `cache/deps.rs:1`); the start's slots as one image linked before `main` (`cache/image.rs:1`: 110 slots in 3.3 ms); everything else on first call | 60,391 MIR bodies made cold; link and codegen tables 238 MiB heap, 133 MiB regions, held |
| the allocator | the process ends | 2,141 to 2,219 MiB freed and kept, 1,200 of it huge pages |

**What Relax gives up.** Every run starts again: process, crate graph (9 to 12 ms), mappings,
link; so its no-op is 52 ms where blaze's is 12, and it writes new images after each edit
(28 ms of the run, 1.8 MB an edit). Its front end is incremental a crate, not an item.
Its memory grows with the project (87 to 130 MB warm and 127 to 212 MB an edit, from 4 to 16
crates) and nothing bounds it. It compiles its own 134k-line std, a function at its first
call, so a dependency's body with an error is found when it runs. Not measured: its live
session (`relax live`) and check server (`relax serve`), where a process does stay.

**What blaze takes from it**: read results in place from immutable files; reach a
dependency only through what the program names; link what did not change without the front
end. **What blaze keeps that Relax cannot have**: no reload on a no-op, results shared by
every project and session of the machine (Relax keeps a copy a target directory), and memory
held to a budget, not to the project's size.

**The strategy.**

1. *Warm, in memory*: the workspace crates' memos (what an edit of them reads and writes), the
   interner's part they name, the running program's patch tables, and the pool's pages read
   since its sweep last came by. Nothing else.
2. *Records only, through the pool on demand*: every sealed crate's front end (interface:
   names to items, signatures, impls by self type and trait; then bodies and MIR by body key)
   and all machine code. A sealed crate is one `HIGH` leaf; its rows have no value on the heap
   and no edges.
3. *Never loaded on a warm run*: a sealed crate's syntax, item trees, expansions, def-map
   internals and the bodies nothing asks for. A sealed crate costs its interface pages the
   workspace reads and the code the program reaches, nothing by its source size.
4. *An edit* pages in the edited file, the workspace rows its change reaches, and the
   interface pages those rows name; the sweep's second-chance bit keeps them for the next.
5. *The budget* is on footprint (rule one-memory-measure): idle, hello-world holds at most
   48 MiB (Relax peaks at 50 a run, 87 resident), of it the pool at most 32; it grows by the
   workspace's own items (about 1 KiB an item), never by the dependencies. Working, the pool's
   share is `Tag::Pool`'s as today. The idle give-back and the squeeze enforce it with the one
   evictor.
6. *The allocator's kept memory*: mimalloc on base pages (done, below); a task's scratch on
   its frame, returned whole; what outlives a task a record, so the general heap holds only
   workspace memos and no page is pinned by a stray live block. An allocator of blaze's own is
   not needed by any measurement so far.

**Order**, each with its measure on `measure.py` (Linux aarch64 VM of 32 cores, home and
project on tmpfs after the VM's memory was fully added: its disk and its memory hot-add each
moved a row by 2 to 6 times until taken out, `agents.json` memstrat-h1):

| Step | What | Milestone | State |
|---|---|---|---|
| a | mimalloc on base pages (`no_thp`) | 1.2 GiB less resident; edit rows within the runs' spread | built |
| c | the pool's give-back has callers: one lap of the sweep each 30 s of idleness, every unpinned page at the limit's line and on a pressure event | the pool's pages of an idle daemon go (66 MiB to 0 after 60 s); no row moves | built |
| b0 | a library's rows not kept as readers (lane records-1111's shed, `2b173f8632`) | 100 MiB live less | measured here, left off: a comment edit 20.0 to 34.7 ms |
| b1 | a sealed crate's tables (`Resolutions`, `ImplSlices`, the crate graph, source roots) as `HIGH` leaves apart from the workspace's; then its rows keep no edges | library rows at `HIGH` (181,865 of 2.1 million today); 320 MiB live less | not built |
| b2 | def maps and impl tables read in place; name resolution's state dropped for a sealed crate | 470 MiB less; under 2 GB | written, not compiled or measured (F2, 2026-11-13): def map pages the one form of a served crate's def map, its names once; impl tables as pages read by trait, by self type and by name; neither record pinned or decoded whole ("Def maps read in place", "Impl tables read in place", below) |
| b3 | syntax, item trees and expansions of a sealed crate not held; signatures not memoized | 660 MiB less; under 1 GB | not built |
| b4 | link and codegen tables as records | 400 MiB less; under 500 MB | not built (the engine's edit path is another lane's) |
| d | restart and cold start read records: F2 and R8, then R7 | first no-op after a restart under 300 ms; then an idle daemon may drop to the budget | not built |

b2 to b4 and d are each a change of what a crate's front end is in memory (ids that mean the
same in every process, tables read in place), a lane a kind of record; nothing short of them
takes the 3 GiB that is live.

**Built** (branch `memstrat-1111`; hello-world, medians, each cell one run; base is main
`3a58d0cbae`; `a` is main with `no_thp` alone; `a and c` is the branch with the pool trimmed
2 s into idleness, as it first was):

| | base, four runs | a, two runs | a and c, two runs |
|---|---|---|---|
| resident, idle after a cold build and five edits, MB | 5,291, 5,397, 5,460, 5,317 | 4,143, 4,159 | 4,095, 4,109 |
| of it transparent huge pages | 1,184 to 1,240 | 80, 76 | 86, 126 |
| of it freed and kept by the allocator | 2,141 to 2,219 | 988, 961 | 972, 978 |
| the pool's pages, MiB | 64 to 67 | 67 | 7, 8 |
| no-op build, ms | 13.9, 13.7, 13.0, 15.7 | 12.2, 13.1 | 13.1, 15.4 |
| no-op run | 18.2, 17.9, 16.8, 19.6 | 17.6, 16.5 | 14.7, 18.2 |
| body edit + run (n = 40) | 108.6, 101.7, 111.2, 124.1 | 111.0, 109.7 | 108.8, 112.5 |
| comment edit + run | 23.4, 22.7, 22.7, 22.7 | 23.7, 22.6 | 20.0, 21.2 |
| body edit 3.5 s after the last | 115.6, 98.6, 117.7, 120.5 | 114.4, 117.4 | 121.2, 129.0 |
| body edit 8 s after the last | 122.9, 106.2, 107.5, 127.4 | 120.2, 127.0 | 115.0, 124.4 |
| first no-op and first edit after a restart, s | 3.8, 10.4 | not run | 4.3, 9.6 |
| resident after the restart's builds, MB | 6,208 | not run | 4,734 |

The pool trimmed 2 s into idleness shows in the 3.5 s row, so the branch as it stands trims
each 30 s of idleness (`serve::idle::POOL_AFTER`): on a live daemon the pool is 66 MiB until
the second lap, 60 s in, and 0 after it. With that wait an edit loop never meets the trim, so
its rows are column `a`'s; the branch's own run of them was cut short by the VM (its first
four rows, before that: 14.0, 15.6, 106.3 and 22.6 ms), and the 3.5 s and 8 s rows of the
branch as it stands are not measured. Step a's price is not in this table: a daemon's edits
96 to 135 are 207 and 210 ms on main (against 110 for its first 40, which no lane has
explained) and 266 and 252 on base pages (`crates/blaze-engine/src/huge.rs` has that table and
what follows if the growth stays).

Tried and left out, each built and measured: a purge delay under mimalloc (300 MB more held,
no row faster); the flight recorder's read room by bytes (the `profile` holder 64 to 76 MiB to
58, a comment edit 22.7 to 26.6 ms).

## Def maps read in place (2026-11-12, lane records-1112b; D21, D29)

Step b2's first half. **Written, not compiled or run by this lane**: the round's results
decide; every number below that is not quoted from another section is a prediction. The one
path: the image holds no def map part, and a served crate is read from its pages alone
(`frozen::served`). Layout 3 (F2, 2026-11-13) writes a module's names once.

**The record** (`crates/hir-def/src/nameres/frozen/pages.rs`): a crate's `DefMap` as one store
value of segments found by offsets from its start, little-endian, nothing fixed up at a read.

| Segment | Holds | Read when |
|---|---|---|
| header (56 bytes), a row a module (64) | the layout, the encoding, where each segment and run is | the crate is first asked for |
| files | each file the segments name: owning crate, path from its root, content ID | with the header |
| head | crate data, the module tree (origin, visibility, parent, children), preludes, whether a scope shadows a primitive type | with the header |
| a module's name table | open-addressed slots (16 bytes: the hash of a name's text, where its entry is), at most half full | a name is looked up in that module |
| a name's entry, in its module's run of entries | the name and its `PerNs`, written once | that name is looked up |
| a module's textual macro table and run of entries | slots as a name table's; the name and its `macro_rules!` | a textual macro is looked up |
| a module's traits | the traits in scope | method resolution asks for them |
| a module's items | its impls, built-in derive impls, declared traits | the impls maintainer is handed a crate with no impl tables |
| a module's scope | the rest of the `ItemScope` (`Enc::scope_rest`), each entry's length, each namespace's names by entry position | something lists the whole scope (a glob): with the two runs, three reads |
| tail | name records, diagnostics, derive helpers | a lint or diagnostic asks |

A lookup of a name (`Modules::name`, the one memo of a path segment) reads a window of four
slots and the entry of the slot whose hash is the name's: two reads of a few dozen bytes, at
most six pages, for a crate of any size. The whole scope is decoded only for a reader of all
of it, a module at a time (`ScopeSlot::Paged`); the records only when asked
(`RecordsSlot::Paged`). The head is decoded once a crate and is the `DefMap` value.

**Through `blaze_cas::direct`.** The bytes come through `PageSource::read(at, len)`. The
driver's (`PagedRecord`, blaze-cargo `serve/unit/deps.rs`) maps a record of
`direct::MAP_FROM` (16 KiB) or more once, verified, and a part is a slice of the mapping: the
kernel's page cache is the one copy, only the pages a lookup touches are loaded, and nothing
is pinned in a pool. A shorter record (or one still in a writer's segment) is read once,
verified, and held: under 16 KiB a crate. A collector keep is taken before the map. The key is
`deps::pages_key`: the image's filed key (every input of the crate and what it read), the
layout and the encoding.

**Ids.** Nothing in the record is a database's id; each kind is written as `nameres::frozen`
writes it, and read by interning again:

| Id | Written as |
|---|---|
| crate | its index in the crate's dependency closure, breadth first |
| file | its owning crate's index and path from that crate's root (`\0ROOT`, `\0OUT_DIR`), once in the files table, with its content ID |
| module | its crate's index and its index in that crate's `DefMap`; a block's by its body's owner |
| item (`FunctionId`, `StructId`, `ImplId`, `UseId`, `ExternCrateId`, macros, ..) | the location it interns: container and `AstId` (file or macro call, and the erased id) |
| macro call | its `MacroCallLoc` whole, the first time a *segment* names it, by index in that segment after |
| syntax context | its outer call, transparency, edition and parents, likewise |
| name | its text; a keyed name with its context |
| a name's slot | FNV-1a of its text (never a hash of an interned id) |

**What it does not do yet.** (1)–(3) of 2026-11-12 are written (F2, 2026-11-13, not
compiled or run): the image holds no def map part and is mapped through `blaze_cas::direct`
(`InStore`, blaze-cargo `unit/deps.rs`), pages likewise (`PagedRecord`), nothing pinned in a
pool; a module's names are written once; impl tables are pages ("Impl tables read in
place"). Checks: `nameres::frozen::pages::tests::a_modules_names_are_written_once`,
`name_resolution_state_stays_zero_across_many_sealed_crates` with its control
`a_sealed_crates_whole_scope_is_seen_by_the_zero_state_check`, and the refinement under
random edits with its plants (a name left out, another name's entry served). Still a crate
record, not per item (rule 65): both are one store value a crate. (4) A workspace crate is never paged: only a
crate with an image is. (5) b3 and d are not built. F2 is not built (below): a crate with records is resolved
from source once, in the write that adds it, before the store is asked.
(6) No new `blaze_cas::Kind`: pages are `Kind::Query` like the image; their keys are what a
pin of "toolchain" or "bstd" names (`pages_key` of each crate's `Image::key`).

**Found while reading for F2 (2026-11-12), which bounds what pages can save today.** After
every crate graph change the name-resolution maintainer hands the impls maintainer a delta of
each served crate made by walking every module's whole scope
(`nameres::delta::served_delta`: `items.add(module.scope(db), ..)`), for every crate whose
impls are not frozen too. With pages that is one `scope_memo` a module of every served crate,
at the serve: the name tables make a lookup cheap, and this walk decodes every scope anyway.
So the next record is the impls and traits of a crate by module (what `served_delta` reads),
written beside the pages and handed over without a scope; until then expect `scope_memo` rows
about equal to the served crates' modules.

**F2 as a set of awaited crates: built 2026-11-11 (5595254297), removed 2026-11-12 (lane
awaited-out; written, not compiled or run by it).** What it was: an input holding the crates
whose records were not asked for yet (`hir_def::deps::awaited`), which both maintainers
passed over, released by the serve that asked about a crate (`serve_asked`), in a front end
lowered with `blaze_lower::lower_awaiting`. Why it went: it was a second scheduling protocol
beside the plane, a held set with its own release events and a promise no type enforced
("nothing reads a crate before a serve asked about it"). The smoke project failed on it three
times with the same error (`no maintained resolution for crate libc .. it waits for the
records of crate libc`, unit `smoke-sys (build script)`), once after a fix of the rule for
what a serve asks about; the cause was never found. The tree is back to what it did before:

- Every crate is resolved in the write that adds it. Read from the code: `lower_writes` adds
  the plan's crates and notes a change of the whole graph, and that write's pass gives a job
  to every crate with no state that no image serves (`nameres::delta::unreached`) and a state,
  a slot and a node to every crate whose impls no record serves
  (`method_resolution::delta::state::prepare`, `run_nodes`), all from source.
- The serve comes after: `Real::lower` (`serve/unit.rs`) writes the ready units' harness roots
  and identities, then `deps::serve` of the ready units. A serve that changes how any crate
  is read notes a change of the whole graph (`hir_def::deps::serve`,
  `Db::note_crates_changed`): a crate that gained an image has its state dropped, and every
  crate still resolved from source has a job again over the decoded maps below it.
- So a restart pays name resolution and the impl tables of every crate once from source
  before the store is asked, and the unserved crates again after the first serve that hits.
  The number on record for this path, from before F2 was built: reth, 827 crates, 191.6 s.

What replaces it is not a held set: a cover per registry crate that seeds a kernel session on
demand (end-state.md), so a crate with records has no job to skip. F2's objective stands; this
path is not how it is met.

**A registry crate's image under any home (2026-11-12, lane records-1112f; written, not
compiled or run by it).** Measured by lane reth-1112 on a hello-world: 80 of 80 dependency
crates refused, kind `encode`. Read from the code: the encoder took an `OUT_DIR` up to its
*first* `/build/` as a checkout's build directory, and the default target directory is
`<home>/build/<name>-<id>` (`blaze_manifest::home::build_dir`), so the home was the place and
every fetched source a file below it. As written: the place is what is before the *last*
`/build/`; the home's fetched sources are a named root (`blaze_usercode::roots::Role::Sources`,
in every build's `portable::Places`), so a registry crate's key names no home
(`crate_root_ported`); `base_db::Port` has the inverse (`placed`) and the one searcher
(`held`, `Roots::embedded`); a literal's paths are written with the port (`Enc::unplaced`)
and read back against the reader's roots (`Dec::placed`; a mark with no port told is an
error); bytes that still hold a root are refused naming it. A file was already named by its
owning crate and its path from that crate's root. A registry crate's *unit* record whose
object code holds a `file!()` path is tied and kept under `Places::here`, as any tied record.
Checks (hir-def `nameres::frozen::tests`):
`a_crate_fetched_under_one_home_is_the_same_bytes_under_another`,
`the_control_a_path_kept_absolute_is_other_bytes_under_another_home`,
`a_literal_that_still_holds_a_directory_is_refused_by_its_name`. To measure: the refused
count on hello-world, 80 to 0.

**What is left, in order** (split 2026-11-12: 1's second half is lane records-impls', 4 is
records-items', 5 and the end-to-end revision cases are records-restart's, bodies and MIR are
records-bodies'; 2, 3, the many-crates count test, the refinement test under random edits and
the double resolve of harnessed crates are records-1112d's, then records-1112e's. F2
(2026-11-13, written, not compiled or run) wrote 3 (the image and the pages mapped through
`blaze_cas::direct`, nothing pinned whole; a module's names once), the many-crates test
(`name_resolution_state_stays_zero_across_many_sealed_crates`) and the refinement of def-map
pages under random edits with its two plants; still to do: the double resolve, the smoke
fix, the component-size measurement for rule 65 (`/tmp/records-1112e/next.md` D), and the
per-item form of both records (rule 65; "Impl tables read in place"). The record keys a pin and a pack export
cover are one list now, storegc's `hir_def::deps::front_keys` / `filing_keys` and
blaze-cargo's `serve::collect::front_keys`: a new record kind adds its key there).

1. *Impl tables as records.* First half written 2026-11-12 (records-1112c, not compiled or
   run by it): a crate's `TraitImpls` and `InherentImpls` as `frozen::encode` writes them are
   kept in the store beside its image (`frozen::key`: the image's filed key and the two
   encodings; written by `frozen::store` from blaze-cargo's `deps::store`, pinned with the
   image for the toolchain and bstd), and given to a database in the write of the serve
   (`frozen::serve`, called by `unit/deps.rs` `images` in one batch with `serve_deps`). A
   crate with tables is `frozen::served`: the impls maintainer gives it no state (`prepare`,
   `run_nodes`; `drop_served` for one it held), name resolution hands no `served_delta` of it
   (`nameres::delta::ImplsServed`, so no scope of it is decoded at the serve), and its readers
   decode its table. The tables go with the image (`withdraw_unimaged`, an implied write
   after hir-def's). Checks: hir-ty
   `method_resolution::delta::tests::tables::a_crate_given_its_impl_tables_is_not_maintained_and_answers_as_the_one_that_made_them`
   with `the_oracle_catches_tables_kept_past_their_image`; the codec's own are hir-ty-mir's
   `tests::frozen`. **Second half: written** (records-impls, then F2; "Impl tables read in
   place"): pages read a key at a time. Left: the per-item form (rule 65), and the whole-table
   readers that section lists.
2. *A slot a crate for what a crate is read from.* **Written 2026-11-12 (records-1112d, not
   compiled or run by it).** Five maps of every crate (`PagedDefMaps`, `FrozenDefMaps`,
   `FrozenLangItems`, `DepImages`, hir-ty's `FrozenImpls`) stay for what runs outside
   queries (the serves, `withdraw`, `withdraw_unimaged`, `served_crates`); each map's `set`
   first gives every crate whose entry moved its own entry, in that write
   (`hir_def::deps::given::give_changed`, crates in order, `HIGH`; kinds `ImageSlot`,
   `DefMapSlot`, `LangItemsSlot`, `PagesSlot`, `ImplTablesSlot`), and every read inside a
   query is of the slot: `deps::image_of`, `frozen::frozen_def_map`, `frozen_lang_items`,
   `pages::paged`, hir-ty `frozen::frozen`. The four maps a serve sets are still set in one
   write. Checks: hir-def
   `nameres::frozen::pages::tests::pages_given_to_one_crate_run_no_row_of_another` with
   `the_oracle_catches_a_gift_to_every_crate`; to measure: none of the five names among the
   durability census's blockers. Known cost, not measured: the first gift of a kind to a
   crate writes the crate's list of slots, which every reader of any of its slots read (one
   re-run of each firewall row, backdated). Not built: the providers
   (`provide::<F>(storage, fn(&Db, Key) -> Provided<Value>)` for `scope_memo` and `tail`),
   which wait for sealgroups' hook.
   *The double resolve of harnessed crates, read 2026-11-12, not fixed*: the first resolve
   is needed (`harness::write` lists a crate's tests from its resolution, after its macros
   are loaded); the second is the root file's group, because the harness is text appended
   to the root file (`Lowered::append_roots`). The fix is in the shape, not the order: the
   appended module as a slot of the crate (`Crate::give`), which name resolution adds to
   the root module as one module more (a delta of one module, `nameres::delta`), so the
   root file's text and its group are not written. Its check: `nameres crate` spans of a
   test crate after lowering, one, with the appended text in the root file as the control.
3. *The image not pinned whole* (records-1112d's; read 2026-11-12, not built). What pins
   it: `ImageStore::get` gives an `ImageBuf` (`Arc<dyn Held>`, `bytes()` the whole value;
   blaze-cargo's `InPool::get` is `Reader::pinned`), and `Image`, `PartBytes::In`,
   `ImageRef` and every signature read borrow slices of it. The port, whole: `Held` gains
   `read(at, len, with)` beside nothing else (as `PageSource`), `Image` keeps its parts'
   ranges and a source, `signatures` decodes an item from one bounded read, lang items from
   one read of their part, `frozen::unreadable` at open reads the files table alone; then
   the image is written without its def-map part for a crate whose pages were written
   (`deps::store`), and the scope segment of the pages names its entries by offset in place
   of writing each name again. Tests owed with it: bytes held only for the read (a
   `Counting` source over an image: pages touched by one signature bounded at two sizes), a
   previous image layout refused by name. Also owed: the count test over many crates with
   records (blaze-cargo's store fixture has one registry crate).
4. *b3*: with 1 to 3, a crate with records makes no syntax, item tree or expansion; what
   still parses is a body asked for (`nameres::demand`).
5. *d*: `deps::key` takes each dependency's record content hash, not its key; the first no-op
   after a restart then reads only the keys' inputs. Bounds to hold: hello-world under 300 ms.

**Checks** (hir-def `nameres::frozen::pages::tests`, blaze-cas `tests::part`):
`a_crate_read_from_pages_answers_as_the_one_resolved` (the refinement: the whole def map and
every name of every module, against the resolved crate) with its control
`the_oracle_catches_a_name_left_out_of_its_table`; `the_same_pages_from_two_databases` with
`the_pages_of_another_source_are_other_bytes`; `a_previous_or_foreign_layout_is_refused_by_name`;
`a_crate_read_from_pages_answers_as_the_one_resolved_under_edits_above_it` (Hegel: random edits
of the crate above) with `the_oracle_catches_a_name_left_out_under_edits`;
`a_lookup_reads_a_bounded_number_of_pages` (40 and 4,000 items: at most six pages each) with
`a_whole_scope_reads_more_pages_than_a_name`;
`a_part_of_a_value_is_that_range_of_it_wherever_it_is_kept` with
`a_part_of_a_compressed_value_is_of_the_value_made_whole`. To measure with the switch on and
off: idle footprint on hello-world and the six-dependency fixture, the edit rows, and how many
`scope_memo` rows ran against how many modules std has (how lazy a real build is).

## Restart reads records (d) (2026-11-12, lanes records-restart, records-restart2; D21, D29)

Step d of the memory table. **Nothing of this lane was compiled or run by it**; what is quoted
as measured is lane reth-1112's (reth, 30 packages, int-1112 `d45e70ddc`, build 43 of its home:
the first build after a restart with nothing changed), and everything else is read from the
code and said so.

### The restart today, step by step

| Step | Where | What it costs on reth |
|---|---|---|
| 1. `daemon start` | `serve/daemon.rs`: read the state, restore the VFS, open an empty database, restore the plane's history | not in the build's numbers. The VFS's Merkle tree comes back whole (every file's and directory's content ID) and the watcher goes on from its saved cursor and stamps, so no file is read or hashed here. The front end's database is not persisted (constraint 2, below) |
| 2. the request's plan | watches, `std library`, `plan` | not split out in what was sent |
| 3. a new database | `Real::lower` (`serve/unit.rs`) with nothing kept: `blaze_lower::lower`, `lower_writes` | read: `files::Mounts::walk` takes every file of every package directory of the plan from the tree with its bytes, and `base_db::Files::set_view_root` hashes every file's bytes again to check the ID the tree gave (`content::hash_many`) and writes each as an input. **The 36,408 inputs written are here, by reading**: a text input a file, each file's source root, each root's file set, each crate's inputs. Bounded by the files of the plan's packages, not by the plan. The split by input kind is asked of reth-1112 (`blaze explain writes build=43`) |
| 4. the keys | `deps::serve` of the ready units: `Walked::keys`, `hir_def::deps::image_key` | a hash a crate over its inputs and (path, content ID) of each file of its source root. It needs content IDs, never text; it gets them from the inputs step 3 wrote |
| 5. the serve | `hir_def::deps::serve` | a crate with an image is decoded; one without is resolved. `key::eligible` refuses a local crate and every crate that depends on one, so **every workspace crate is resolved, expanded and item-treed from source at every restart** (read). Measured self CPU of build 43: `impls warm impl` 40.5 s, `nameres collect_expansion` 19.0, `parse_macro_expansion` 17.3, `nameres resolve_round` 16.2, `nameres crate` 14.5 (19,941 spans), `nameres update` 13.8, `impls apply_impls` 13.5, `macro_expand` 8.8, `nameres records` 8.5, `file_item_tree_query` 8.2. Which of the 19,941 are workspace crates is asked |
| 6. harness roots, identities, then a batch of writes for each build script's run and proc macro's load | `harness::write`, `identify`, `UnitWriter::apply` (`ran`, `macros`, `serve_written`) | each is a write that opens a view, and each row read after it is proved again. 144,686,333 verified over 6,813,023 executed is 21 a row: by reading, the views the build opens times the rows later readers re-prove, with no seal held (seals are memory only, and no host seals in a real build yet). The count of views and `verified` by query are asked |
| 7. each unit's record | found by a key that needs the unit's identity, which needs its crate resolved | 0 units made; every workspace crate was resolved to learn that |
| 8. the walk | describes | `body_query` 55.3 s self CPU (215,072 spans): R7, not this lane's |
| 9. what is lost | `noop::LastAnswer`, the seals | the next no-op is a build again |

191.5 s, under a load of 45; `wait` spans sum to 91,235 s of wall and `impls warm impl` to
86,214 s, so most of the wall time is workers waiting on the impls maintainer (not explained
here). Memory, resident from `/proc` (not footprint, which was not read at these points):
46.7 GiB before the stop, 19.0 GiB after the restart's build. So on reth the restarted daemon
was smaller than the one stopped; the claim that a restarted daemon is larger is hello-world's
(6,081 against 5,143 MB, above) and is not shown for reth.

**Corrected by reth-1112's rows of build 43 (measured; this replaces what steps 3 and 6 of the
table say "by reading" about the 36,408).** An input made is not counted as written
(`query_counts.written` counts a set of a field after the input exists), so the files a new
database is given are not in the number at all: `FileText` is 212 of it. The 36,408 by kind:
`PathBucket` 15,119; `Crate` 7,602; `InherentSlice` 4,375; `ScopeInput` 2,793;
`SourceRootInput` 2,266; `CandidateSlice` 1,061; `LibraryResolutions` 1,059; `TraitImplsSlice`
905; `CrateResolution` 451; `SliceDirectory` 329; `FileText` 212; `Resolutions` 97;
`ProcMacros` 54; `AllCrates` 34; `DepImages`, `FrozenDefMaps`, `FrozenLangItems` 13 each;
`KeptConsts` 12. So they are sets made *again* during the build: path buckets and source roots
as packages and `OUT_DIR` files are added, crate inputs as runs are applied (`Crate.cfg_options`
of libc reran 10,444 memos, of object 9,899 and 9,898, of std 6,183), and the two maintainers
publishing scopes and impl slices of the crates they resolved (827 distinct crates had a
`nameres crate` span, 19,941 spans; reth's own crates 33 to 49 spans each, `reth_rpc_api`
17.5 s). The rows verified most are expansion and item-tree rows: `parse_macro_expansion`
18.3 M (753,647 run), `ast_id_map` 18.3 M (1,049,524), `macro_arg` 16.6 M (558,793),
`file_item_tree_query` 11.4 M (419,450), `AttrFlags::query` 11.1 M, `Modules::name` 8.5 M,
`ModuleScope::legacy_macro` 6.6 M, `Body::of` 5.7 M. `maintain` 2 spans (23.9 s wall),
`maintainer reads` and `maintainer apply` 204 each; `lower workspace` 51.1 s wall, 5.9 s CPU.
The number of views opened was not obtained. Builds 45 and 47, nothing changed, same daemon:
3.7 and 2.6 s.

What this changes in the design below: nothing in its pieces, but its reasons and its expected
numbers. The cost of step 3 is real work (every file read and hashed twice, 51 s of wall in
`lower workspace`) that the written count never showed, so A to D are judged by that span and
by `PathBucket`/`SourceRootInput` sets (17,385 of the 36,408), not by `FileText`. The 827
crates resolved are far more than reth's workspace: crates with no image at all (not eligible
because a proc macro's library was not known when they were keyed, or above a local crate, or
refused), which is asked of the profile before E is sized. The expected "about seven a crate"
is a guess at sets, and creations are not counted: the bound to hold is that no `PathBucket`,
`ScopeInput`, `InherentSlice`, `TraitImplsSlice`, `CandidateSlice` or `CrateResolution` is set
for a crate found by its records.

### The restart as it must be

The rule: *a restart, a second checkout and a cold start with a filled store compute, for each
crate, a key from the plan and from content IDs the VFS already holds; a crate whose records
are found under it is read from them and nothing of it is read from disk, hashed, parsed,
expanded or resolved; a crate whose records are not found is resolved from source, it alone.*

**A. The file-hash index is the VFS's tree, which is already persisted and verified** (the
image's tree section and the journal; a file's ID is the hash of its bytes, a directory's the
hash of its entries; the watcher re-reads only a file whose stamp moved). No second index is
built. Two things were missing and are this lane's: a directory's ID without its entries
(`blaze_vfs::Pinned::dir_id`, `blaze_query::Tree::dir`), and not hashing again what the tree
vouches for: `set_view_root`'s check of every file's bytes against its ID goes for a file the
tree gave (the tree is the proof; a `MemTree` in a test keeps the check).

**B. The plan key** (`blaze_lower::found`, new). A hash, by domain `blaze v2 found key 1`, of
what the plan says of a crate: the fields `hir_def::deps::key::crate_inputs` hashes, written
with its markers so no path of the machine is in it; of each directory its package loads, the
directory's Merkle ID and, for each path under it the load skips (`target/`, a nested package,
another crate's `OUT_DIR`), the path; the plan keys of its dependencies in the crate's order;
the front end's code identity. It reads the plan and one tree node a directory. A package
whose listing holds a symlink has no plan key (the refusal names the link): what a link reaches
is not under the directory's ID, and such a package is lowered as today.

**C. The found record** (`Kind::Query`, key `found_key(plan key)`, beside `deps::pages_key`;
layout word first). It holds: the key the crate's image is filed under; the source root's
manifest, each file's path under the package and its content ID, sorted by path; cutoff-1112's
surface digest of the crate (`Lowered::surface_of`), which the plan key covers since it covers
every file, the settings and each dependency's plan key. Written once a crate's image is
(`store kept front end`); read by the lowering. A record under the key that does not read, or
is of another layout, stops the lowering naming the crate and the layout. Two databases of the
same sources write the same bytes: nothing in it is an ID of a database.

**D. The decision** (`lower_writes`). For a package each of whose crates in the plan has a
found record, the source root is *declared* from the manifest
(`base_db::Files::declare_view_root`: a file ID a path, the content ID as the file's content,
no text), the crates are made with no job (this step was written over the awaited set, removed
2026-11-12: it waits for end-state.md's covers), and the serve fetches each image by the filed
key the record gave. A declared file's text is read from the tree by its content ID the first
time something asks for it (a body: `nameres::demand`); a tree that no longer holds that ID is
an error naming the file, never an empty text. For any other package: today's path.

**E. Workspace crates have records.** `eligible`'s refusal of a local crate and of a crate
above one goes: it kept the store from churning under edits and was never needed for
correctness, since a key covers every file. In its place, two decisions the driver makes:

- *When a local crate's image is written*: when it is quiet. That is when sealgroups' pass
  seals it (the second build in a row with no write to its inputs), and at `blaze stop` and
  at the idle give-back for every crate with no write since its last resolve, so a stop after
  an hour of edits leaves records for all but what was being edited. `deps::store` takes the
  crates to write (`write: &[Crate]`, sorted).
- *When a local crate is served from its image*: only when it was found by its plan key at a
  lowering (a restart, a new checkout). A crate resolved in this process is never swapped for
  its image, so an edit loop never meets `withdraw_moved`'s pass over every crate for a crate
  it is editing. A served local crate that is then edited is withdrawn once, in that edit.

A dependency enters a crate's key as its surface digest (cutoff-1112; given to the database
as a slot a crate, read by `image_key` as a tracked read), not as its key. So after a restart
an edit of one crate's body resolves that crate and no other: its dependents' keys did not
move. A signature edit moves the digest and the dependents that read it.

**F. Units.** A unit's record key takes, for its front-end part, the image's filed key the
found record gives, so a no-change restart finds every unit's record with no crate resolved.
The form is content-keys-1112's; until it lands, `identify` resolves a unit's crate from its
image, which the pages make cheap but not free.

**G. Seals and the no-op answer.** A crate served from records by its plan key starts sealed:
its seal's imports are its dependencies' records, named by their keys, which the plan key
already covers, so the seal needs no stamp from the stopped process. The census (groups, rows
behind seals) is then true from the first build. A kept no-op answer can be a record only with
F3's condition (each watch it rested on vouched again at the start), which is durable state
deciding a build: it lands with its simulation, in noopkeep's lane, not here.

**Expected, to be replaced by the runner's numbers.** A restart with nothing changed writes
inputs by crates and roots, not files: about seven a crate (data, extra data, cfg, env,
workspace data, its root's file set, its image slot), two a root, and the
singletons. hello-world (27 crates): about 250, against about 5,000 today (5,087 was measured
for a change of plan). reth: about nine a crate of its plan, independent of its 36,408; the
crate count of its plan is not known to this lane. Name-resolution, expansion and item-tree
queries run for a crate with a found record: 0. Units made: 0.

### State (2026-11-12)

Written, none of it compiled by this lane (lanes records-restart, then records-restart2):

- `harness/v2/restart_rows.py` (the rows above for hello-world, the six-dependency fixture and
  the smoke workspace); the revision harness's `max_ran`; the cases
  `a_restart_with_nothing_changed_resolves_nothing`,
  `a_restart_then_an_edit_in_one_crate_resolves_that_crate_only` and
  `a_copied_store_in_a_new_home_resolves_nothing`, each bound that fails today listed as known
  with this section as its cause. Their bounds (`max_written = 400`) are guesses until the
  runner sends numbers.
- *A, whole.* `blaze_query::Tree::dir(root, path) -> Option<ContentId>`, required, on its five
  implementors (`blaze_vfs::Reader` by `Pinned::dir_id`, the node's own Merkle ID; `MemTree`
  by a hash of the files under the path, `MEM_DIR_DOMAIN`; `Breaking`, `Backwards`;
  `ProbeFiles`, which holds no IDs and reads to make one). Not done of A: `set_view_root`
  still hashes every file again (it goes with D's `declare_view_root`).
- *B, whole and called by nothing.* `blaze_lower::found` (`crates/blaze-lower/src/found.rs`):
  `dir_key(mounts, dir, skipped)` and `plan_key(&Said)`, with `NoPlanKey` naming why a crate
  has none. Two things differ from B as written above, on purpose. (1) A directory's key
  leaves out what its load skips: a package directory holding `target/` or a nested package
  would otherwise move its key with every build or every edit of the nested package; a
  directory with nothing skipped under it is one tree node, any other is listed along the
  paths to what is skipped (and to a mount below). (2) Symlinks: a link in a listed directory
  refuses the key, but a directory taken whole by its ID is not listed, so the *writer* of a
  found record must refuse a package whose walk followed a link (`files::Mounts::walk` does
  not report that yet: a `followed` flag is the next writer's, with the record's write).
- *C, the record whole, its write and read not written.* `hir_def::deps::found`
  (`crates/hir-def/src/deps/found.rs`): `found_key(plan key)`, `Found { filed, families,
  own_surface, files }`, `encode`/`decode`, `FoundError`. `families` (asked by records-items)
  says which record families the writer put under the filed key (`family::IMAGE`,
  `DEF_MAP_PAGES`, `IMPL_TABLES`; `ITEM_PAGES` retired 2026-11-13, kept known so old records
  decode), so an image older than a family is a miss of that family. `own_surface` is cutoff-1112's 101 bytes (`Lowered::own_surface`, given back
  by `Lowered::carry_own_surface`), not the folded digest, which dies with any change below
  the crate. Not written: the put in `serve/work.rs` `store_kept` (one put beside the image;
  noopkeep lists that file and wants the lines first; memgive too), which also queues the key
  for the pins (packs-1112: `crate::serve::collect::keep(TOOLCHAIN | BSTD, [Key(found_key)])`
  for std and bstd crates, at the write AND when a build finds the record), and the read.
- *Beside E.* `hir_def::deps::why_not_eligible(db, krate) -> Option<Ineligible>` (`key.rs`):
  the rule of `eligible` that refused a crate and the crate it was met at. Nothing reports it
  yet; see "The 827" below. `PlanBase`: `key.rs`'s markers for a crate in no database.

**A found record is a claim, not proof (decided while writing B; changes D and F).** A plan
key is made before any build script's run is applied, so it cannot cover what a run gives a
crate (cfgs, variables, `OUT_DIR` files). So: the serve still makes the crate's `image_key`
from the database (content IDs only, which declared files have; no text) and asks the store
by *that* key; the record's `filed` and `families` are used only where they agree with it. A
record that names another key is a miss, never an error and never an image of other inputs.
For the same reason a crate a run applies to, and every crate above one, takes no carried
surface (cutoff-1112's rule), and F's "unit key from the filed key" holds only for crates
with no run below them; for the others the unit key takes the `image_key` made after the run.

**A simpler D to weigh before building it as written.** `Tree::file` already gives a file's
content ID and a handle on its bytes without reading them (the tree's `Arc` or a range of
the mapped image), and `Mounts::walk` reads no bytes itself: by reading the code, the cost of
step 3 is `set_view_root` hashing every file again and making a text input of each. So
`declare_view_root` (path -> content ID, text on first read) can be used for *every* package,
found record or not, from today's walk: a crate with no record then reads only the files it
parses, and the manifest in the found record is needed only to skip the listing (one node a
package in place of one a directory). Not measured: which part of `lower workspace`'s 51.1 s
wall (5.9 s CPU) is the walk and which the hashing; ask reth-1112 for the spans under it
before choosing.

**The 827 (reth-1112, build 43; not explained).** The profile holds no reason: `Stored.refused`
is read by nothing, and reth-1112 has added an event at the push (on its branch). What it
does hold: five serves asked 318 crates in all and served 291 to 299; so most of about 1,100
crate instances were never asked, which is eligibility, not refusal. By reading, the
candidates are `Ineligible::AboveLibraryNotKnown` (a crate above a proc macro whose library
was not known when it was keyed: most of an ecosystem is above a derive) and `AboveLocal`.
The next step is a count by `why_not_eligible(..).kind()` at the serve, as an event field.

Not written: C's put and read; D (`Files::declare_view_root` in base-db `view.rs`, the
decision in `lower_writes`, dropping `set_view_root`'s second hash); E's switch (the two
`is_local` terms in `key.rs`, `deps::store(.., write: &[Crate])`, the quiet-crate and
stop/idle writes); F; G; the simulation of a restart at every point of a build; the measured
bounds. One more for E, by reading: `crate_inputs` hashes
`proc_macro_cwd` through the package's markers only, and a member's is its workspace root, so
a member's image key holds this machine's path until that is written as a marker too
(`a_copied_store_in_a_new_home_resolves_nothing` will show it once members are eligible).

What the next writer needs, agreed with the lanes named:

- *D.* base-db `view.rs` has no owner: `Files::declare_view_root` is this lane's; a declared
  root's files join the root's seal group as sealgroups' hunks in `files.rs`, `input.rs` and
  `lib.rs` do for a set one (tell it the lines). In `lower_writes` the crate's fields are
  made after its root file is looked up in its source root; for the decision they must be
  made first (`found::Said` lists them), and a macro-host unit reads its root's text
  (`crates::links_proc_macro`), one file, before its cfg is known. onefront and the records
  lane holding blaze-lower's `lib.rs` are told the lines first.
- *C's put, agreed.* Families: `hir_def::deps::Stored::families` (records-items; the same for
  every crate of `Stored::written`), with `family::IMPL_TABLES` ORed in where hir-ty's
  `frozen::store` put them. Pins (storegc): in `serve/unit/deps.rs` `store(...)`, chain
  `blaze_cas::Key(found_key(plan))` into the pin block's `crate::serve::collect::keep(name, ..)`
  call for each std/bstd crate, both for crates this build found by their record and those it
  put one for; `front_keys` does not cover found keys (they are not made from the filed key),
  so the writer owes a test that every found key put for a std/bstd crate is queued to its
  pin, or a record written and not pinned is collected. A found record without its image, or
  the reverse, is a miss either way.
- *Named roots (records-1112f; written, not compiled), owed by this lane, not written.*
  `base_db::Files::ported` now has its reverse `Files::placed(bytes)` and `Files::held(bytes)
  -> Option<&'static str>` (the named root a path is still under; identity and `None` while
  no port is told), and the home's fetched sources are a named root (`Role::Sources`, tag
  `f`). Two uses here. (1) The found record's manifest holds only paths under the package
  (layout 1 refuses any other), so a file of the source root outside the package (a target
  root elsewhere) is left out today: give the record a second path form, written with
  `files().ported` and read with `files().placed`, with a layout bump. (2) `key.rs`'s
  `Base::value` writes any other absolute path as the one marker `\0ABSOLUTE`, so two
  different outside paths collapse and nothing says which root one was under: write
  `files().ported(path)` in its place and ask `files().held` in `PlanBase::holds_machine_path`
  and `Base::held_by`; that moves every image key (a new `KEY_DOMAIN`), so it goes in one
  hunk with E's switch, agreed with cutoff-1112 who also edits `key.rs`.
- *E, the surface slot (cutoff-1112, 2026-11-12; written, not compiled).* `image_key` takes a
  dependency by `given::surface` where one is given, else by its key (key domain 7); the
  lowering gives every crate of the plan its surface (`Lowered::give_surfaces`) at the end of
  each write that can move one, in that write's batch (`refresh`, a run applied, proc macros
  loaded, a root appended) or right after it (`relower`), and the driver once after
  `set_port`. The three questions, answered by reading: (1) a surface reads inputs and parses
  alone, nothing a maintainer keeps, so it is worked out inside an open batch; (2) a registry
  crate's narrowed identity is given unit by unit after the first serve, so the slot never
  holds it: a library crate's slot is its whole identity always (`slot_of`), the same in the
  build that keeps an image and the restart that looks for it. The price: a workspace crate
  above a registry crate that calls a function where a constant is evaluated keeps its
  positions in its slot, so a moved line in it moves the image keys above it (a body edit
  that keeps its lines does not); closing it needs the kept narrowed identities read before
  the first serve; (3) `Crate::give` writes the slot and notes nothing to the maintainers, so
  a crate given a surface gets no job for it. Tests:
  hir-def `a_dependency_enters_the_key_above_it_by_its_surface_where_it_was_given_one` (its
  first half is the control), blaze-lower
  `every_crate_is_given_its_surface_and_what_no_front_end_reads_moves_none`. Known hole, older
  than this: a file `include_str!` reads outside its crate's source root is in no surface.
- *E.* records-1112d handed `hir-def/src/deps/key.rs` over and holds the writer to: `eligible`
  and `image_key` drop `!data.origin.is_local()` and keep `libraries_known`;
  `deps::store(db, served, images, write: &[Crate])` (callers: `serve/unit/deps.rs` `store`,
  where storegc's pin block is, and seven in `deps/tests.rs`); the tests
  `only_crates_outside_the_workspace_are_eligible` and `a_library_on_a_member_is_not_eligible`
  assert the old rule and are rewritten with it; a local crate's slot (`deps::given`) is
  written at `HIGH`, so it is written when the crate is found by its plan key, never per edit.
- *The digest.* cutoff-1112's `Lowered::surface_of(krate) -> ContentId` (blaze-lower, written,
  not compiled), not the image's content hash: an image does not hold a `macro_rules!` body or
  a `const fn`'s body that a dependent reads on demand. hir-def cannot call blaze-lower, so
  the lowering gives it to the database as a slot a crate before keys are asked, and
  `crate_inputs` reads the slot of each dependency in place of its key; cutoff adds the call
  once the setter is named. The found record may carry the digest because the plan key covers
  the crate's files, settings, root spelling and every dependency's plan key.
- *Keys told to storegc and packs*: `found_key`'s form (packs was told it is coming).

## Items read in place (b3; 2026-11-12, lane records-items; D21, D29)

**Written, not compiled or run by the lanes**: the round's results decide, and no count here is
a measurement. One path (2026-11-13, F3): the crate-keyed item pages (`frozen/items.rs`,
`items_key`, `ITEM_PAGES`) are deleted; every row below is per-item entries through
`nameres::frozen::entries`, which the driver installs over the store
(`serve/unit.rs` → `deps::entries`, read through `serve::direct`).

**What forces a served crate's syntax** (read from the code). No reader of a served crate asks
for an item tree: `file_item_tree_query` is called by name resolution's collection alone, which
runs once for every crate in the write that adds it (F2, not built). What is left makes a whole `parse`, a `file_outline` (a copy of the parse)
or a `parse_macro_expansion` (with `macro_expand` and `decl_macro_expander`, which parses the
defining file):

| | Queries | State |
|---|---|---|
| A | `ImplItems::of`, `TraitItems::query_with_diagnostics` (the impl's or trait's file, and each macro call at assoc position expanded) | per-item entries (`entries/items.rs`) for a `plain` impl or trait; one with a macro call at assoc position is collected from syntax until the expansion key (D) is built |
| B | `AttrFlags::query` (`file_attr_flags`: the file's outline; a macro-written item: its expansion) | per-item entries for a file-written item; a module's flags and a macro-written item's are made from syntax (not built) |
| B | `field_attr_flags`, `generic_params_attr_flags`; the 33 attribute value queries of `attrs.rs` (`link_name` .. `block_disabled_sanitizers`, `docs`, `cfgs`, `repr_assume_has`, `target_features`, ..); stability (`own_of_def`, `own_of_fields`, `default_body_of`, `deprecation_origin`, `stability_origin`) | per-item facts (`deps/facts.rs`, family `attr value`, keyed by the item's `attrs`): `symbol_attr`'s five, `lang`, `target_feature`, `cfg`, `unstable`, `must_use`, `rustc_must_implement_one_of`, `rustc_legacy_const_generics`, `wasm_import_module`, `repr`, `define_opaque` (paths of root names), `sanitize` (the sets as written; what an item inherits is applied on read), `link` (every `#[link]` with its `cfg(..)` as written; the crate's cfg picks on read, so no cfg is in the key); field flags keyed by the variant's `header` (2026-11-13, F3, not compiled); not built: `docs` (its value holds file ids and a source map: an IDE read, no build reads a dependency's), stability, generic-param flags |
| B | crate-wide walks (`crate_diagnostic_items`, `crate_notable_traits`, `UnstableFeatures::query`, `ImportMap::of`) | not built: no table a crate (rule 65); each walk reads the per-item facts above and the item flags, so a sealed crate's walk is syntax-free once every fact it reads is one; a crate root's own attributes (a module's) are not kept |
| C | `enum_has_unsafe_fields`, `written_self_view`, `extern_block_abi`, `get_constrained_lifetimes`, `written_visibility` under `assoc_visibility`, `ExternCrateId::alias_or_name`, macro names | per-item facts keyed by the item's `header` (`deps/facts.rs`; 2026-11-13, F3, not compiled); `get_constrained_lifetimes` reads only the served signature |
| C | `with_source_map`/`relative` of each signature | lowers again from syntax by design; whether only a diagnostic of that item reaches it is not checked (the callers are the dispatchers in `expr_store.rs`, `hir/generics.rs`, `lib.rs`) |
| D | `decl_macro_expander` (the tokens of a `macro_rules!`/`macro` a dependent calls) | per-item entries (`hir_expand::declarative::stored`, layout 2), written, not compiled or run; a definition with a `cfg_attr` is kept with the crate's cfg in its tokens' key; one a macro wrote and one whose spans have a wider anchor are made from source as before. The expansion key (`stored::expansion::key`: the layout, depth, the definition's edition, flags and header or parent key, the call's kind, `expand_to`, its site's header and body or parent key, the calling crate's edition and cfg) is built (2026-11-13, F3, not compiled): an expansion's items have a file index under it and their facts are found with no expansion; no key (made from syntax) for an attribute macro, an eager call, a proc macro, or a builtin that reads its position (`column!`, `file!`, `line!`, `module_path!`) |
| D | the hygiene keys of `nameres/def_site.rs` (`macro_def_module`, `expanded_item_key`, `use_tree_key`, `module_item_key`, `expanded_*_context`) | not built: their values are interned `SyntaxContext`s; keeping them needs a portable context (its marks named by expansion keys) and, in the reading database, a map from an expansion key back to its call |
| D | `include_macro_invoc` (decoded every scope) | from syntax (the crate-keyed list went with the item pages); one entry a call under the expansion key, not built |
| E | an item's range or line (`method_resolution::may_hold`, `opaques.rs`, hir-ty-mir `location.rs`, hir-ty-codegen `places.rs`, `const_check/stability.rs`), blaze-items' `item_key` and macro token forms (`key.rs`, `form.rs`), lint levels (hir-lint-levels) | positions are a segment of the file index (layout 3: an item's start and end after the assoc pairs, read by `FileIndex::range`); `may_hold` and `opaques.rs` read it (2026-11-13, F3, not compiled). Not ported: `location.rs`, `places.rs` and `const_check/stability.rs` place a body's node (records-bodies); `key.rs` and `form.rs` read no syntax; lint levels not built |
| F | bodies: `Body::of`, `body_syntax`, `block_item_tree_query`, `block_def_map`, `nameres::demand`, MIR's `parse_macro_expansion(..).err` | lane records-bodies |

**Written when** the crate is first resolved from source, in `deps::store`'s task of that
crate (`deps::facts::keep_all`, called by `serve.rs`'s `make` where the item pages were
written): every module's items (blocks left to their bodies), their assoc items, variants,
macros, extern crates and blocks, each fact a dependent asks, so the entries are kept while
that database's parse is in memory. A database's own asks keep them too (`fact` puts on a
miss). An entry the store dropped is a miss and made from syntax, never a wrong answer: the
per-item entries are not among `front_keys`, so the collector's pins for them are storegc's
(not built).

**Checks** (hir-def `deps::facts::tests`, end to end: `deps::store` writes `lib`'s image and
facts from a source database, a second database is served the image by `deps::serve` with the
same entries): `a_served_crate_answers_every_fact_with_no_syntax` (no query named `parse`,
`item_tree`, `macro_expansion`, `outline`, `expander`, `macro_arg`, `ast_id_map` runs; a
method's self view and an enum's unsafe fields among the facts, their signatures read from the
image) with `with_facts_read_live_the_same_asks_make_syntax`;
`one_item_of_n_edited_makes_its_fact_alone` with `keyed_by_file_one_edit_makes_every_fact_again`;
`facts_of_a_previous_layout_are_made_again` with its same-layout control. Not written: the
blaze-cargo test that a dependent's build runs none of those queries for a served crate.

**State and what is left** (2026-11-12, lane records-items2; nothing below was compiled or run
by this lane). The crate-keyed record (`items_key`, deleted 2026-11-13) was against rule 65 and
is replaced by per-item entries. The per-item form has three levels, none keyed by
a crate: a file index (ast id to `ItemSource { attrs, header, body }`) under the file's content
and how it is read; entries under their own keys, all through
`nameres::frozen::entries` (`entry`, `keep`; the driver's `Entries`); a per-crate table of item
key to entry key that is an index only.

Built in that form: macro definitions (`crates/hir-expand/src/declarative/stored.rs`, reached
from `decl_macro_expander`; hir-def's `entries::set` installs the bridge to the entry store).
Two entries a definition: the source row, H(file content, edition and extensions, ast id) to
the definition's position-free tree hash (the file index's row for this item, one record a row
until the whole-file index exists); the tokens, H(tree hash, ast id, edition and extensions,
defining crate's edition) to the transparency and token trees. The tokens name no file, crate
or context: every span of a kept definition is of its file's root context and relative to the
definition's own ast id, and a definition that is not so is not kept. An unchanged file: two
gets, no parse. An edited file: its parse, one row put a macro asked, and the tokens of every
unchanged macro found. `stored::def_hash` is the definition's content hash for other keys (a
body's macro-definition fact). The driver calls `entries::set` (`serve/unit.rs`, a fresh
unit's database, over the store through `serve::direct`).
Checks: hir-expand `declarative::stored::tests` (round trip, a changed token, a span not its
own is not kept, a layout refused by name, the key's parts); hir-def
`nameres::frozen::entries::tests`: `a_kept_macro_definition_is_expanded_with_no_parse_of_its_file`
with `without_kept_definitions_the_same_asks_parse_the_file`, and
`one_macro_of_n_edited_finds_the_tokens_of_the_others` (N-1 found; the edited one a miss).

**State (2026-11-12, lane records-items3; written, not compiled or run by this lane).** The
file index is `crates/hir-expand/src/declarative/stored/index.rs`: one entry a file under
H(file content, edition and extensions, layout), a row an item, assoc item, extern item, variant
and the file, by ast id: `ItemSource { attrs, header, body }` and three bits (`attrs_cfg`,
`header_cfg`: the hashed text names `cfg`; `plain`: an impl or trait whose assoc items take no
name's lookup). The hashes are of text (kind and text, bodies cut by range, the inner attributes
of a cut body kept in the header), not `FlatTree::subtree_hash`, which hashes a lazy body and a
parsed one differently: an outline and a full parse make the same bytes. Found by content with
no parse; the macro source row is this index's row (`def_hash` is its `header`).
`BodySyntax::item_hash` is replaced by `header` and `body` by lane records-bodies4.

Entries of an item are `crates/hir-def/src/nameres/frozen/entries/items.rs`, each a cache
before its query (found: nothing parsed or collected; not found: the query runs and its answer
is kept; no per-crate encode pass). `attr flags`: H(kind, how the file is read, `attrs`, the
crate's cfg only when `attrs_cfg`), every file-written item but a module. `assoc items`:
H(impl or trait, how the file is read, `header`, the cfg only when `header_cfg`), only for a
`plain` impl or trait; the value is each item's kind, name and place among the assoc items
written (layout 2, lane assoc-key, 2026-11-12, written and not compiled by the lane), and the
reader interns, under the container it asked of, the ast id its own file's index lists at that
place (`FileIndex::assoc_items`, index layout 2: a pair an assoc item after the rows). Layout 1
held the ast id itself, which is of its file (an impl's id counts the file's impls of its self
type and trait before it): two impls written the same, as `once_cell`'s in `unsync` and `sync`,
were two values under one key, refused within a build and read as each other's from a kept
store (`agents/annoyances/assoc-key-1.json`). The rule for every entry keyed by an item's text:
its value holds nothing of a file or a database (no ast id, no crate, no interned id); what is of
the file is in the file's index. Checks: hir-expand `declarative::stored::index::tests`;
hir-def `nameres::frozen::entries::items::tests` (one impl of N edited: N-1 found and one
collection; keyed by the file: all N miss; a body edit: no entry made again; two databases:
equal bytes; a layout refused by name; two impls written the same read their own items, make
one value within one build, and `the_oracle_catches_an_ast_id_in_an_assoc_value` plants layout 1's value; an entry is the
same bytes wherever its item is written).

Left, in order: resolution facts ((module, path) to macro definition, absent included) and the
expansion key, so impls with macro calls, attribute macros or tool attributes, items a macro
wrote, and `include!` calls (one entry a call) are kept too: until then they run as before;
the cfg part as the truth of the atoms named, not the crate's whole cfg; module flags (the
declaration's row, the file's row, the command line's files); the per-crate index (item key to
entry key: with entries found under their own keys it saves only the hashing); the index
moved to `hir_expand::file_index`. Done 2026-11-13 (F3, not compiled): `items.rs`, `items_key`
and the served branches deleted; the driver's `entries::set`; the end-to-end no-parse check
(`deps::facts::tests`); rows B, C, E as the table says.

## Impl tables read in place (2026-11-12, lane records-impls; D21, D29)

The second half of item 1 of "What is left" above. **Written, not compiled or run by this
lane**: the round's results decide. No switch: the whole-decode record of the first half was
never built or run, so it is replaced, not kept beside. A crate with an image and no tables in
the store is maintained from its def map as before (`frozen::serve` gives it nothing).

**The record** (`crates/hir-ty/src/method_resolution/frozen/pages.rs`, layout 2 since F2
2026-11-13, key `frozen::key`: the image's filed key, the layout and the two encodings,
`Kind::Query`): a header of 100 bytes (five tables and a flags word: `DECLARES_SEALED`,
whether the crate declares a `#[blaze::sealed]` trait), the files table, then a body of segments
(`hir_def::nameres::frozen::segments`, new: `Writer`, `files`, `reader`, the generic half of
the def-map pages' segment code) and four open-addressed tables of 16-byte slots, at most half
full. Every place is from the body's start; nothing is fixed up.

| Table | Key | Entry | Reader |
|---|---|---|---|
| rows | trait, simplified self type | the impls and built-in derives | `pages::row` |
| traits | trait | its blanket impls; where its rows are (one run) and each one's length | `pages::blanket`, `pages::trait_into` |
| inherent | simplified self type | the inherent impls | `pages::inherent` |
| names | a name's text | each (name, trait, function or constant) of the crate's traits | `pages::value_items` |
| selfs | simplified self type | where each row of that self type is (rows not copied) | `pages::for_self_ty`: k+2 reads for k rows |

A slot's hash is of what the key is in every database: FNV-1a of the item's syntax id
(`ErasedFileAstId`, its kind and name), of a simplified type's kind and number, of a name's
text. It only finds the slot; the entry holds its key as the frozen encoding writes it, which
the reader compares. A lookup reads one window of four slots and one entry. A key that is not
there ends at an empty slot of its window: a definite "none" from one read, and a table with
no key answers from the head. Every reader calls `report_stored_read`.

**Readers** (all in `method_resolution.rs` unless said). Of one key, for a served crate:
`TraitImpls::in_crate_and_deps_for_trait_and_self_ty` and `in_crate_for_trait_and_self_ty`
(a row), `blanket_in_crate_and_deps` (a trait's entry), `InherentImpls::in_crate_for_self_ty`,
`in_crate_trait_value_items`, `delta::read::impl_row_hash` and `blanket_hash`
(`TraitImpls::served_row`, `served_blanket`). Of one trait: `for_crate_and_trait` (the trait's
entry and its rows' run). Of everything, the whole table decoded: `TraitImpls::for_crate`,
`InherentImpls::for_crate`, `trait_value_items` (which `encode` and `Mode::Verify` read).
Ported after the cut of 18:24 UTC (written, not compiled): `where_const::bound_refusal`
(`TraitImpls::each_blanket_in_crate`, new), the solver's arm for an unresolved type variable
(`next_solver/interner.rs`: the blocks, then `for_crate_and_trait` of each loaded crate),
hir-ty-coherence's overlap walk and hir's `Impl::all_for_trait` (`for_crate_and_trait` a
crate), hir-ty-suspend `root.rs` and hir's two walks of inherent impls for a type
(`InherentImpls::for_self_ty_in_crate_and_block`, new, over `in_crate_for_self_ty`),
`hir/diagnostics.rs` and blaze-diagnostics' `has_impls` (the trait's table of the checked
crate). hir-ty-pure's four sites in `models.rs` and `effect_attrs.rs`
(`in_crate_for_self_ty`, `for_crate_and_trait`). F2 (2026-11-13, not compiled): hir's
`Impl::all_for_type` reads `TraitImpls::in_crate_for_self_ty` (the selfs table) and
`traits.rs`'s `declares_sealed` reads the header's flag, for a served crate. Still a
whole-table read of a served crate: `for_crate_and_deps` and `for_each_crate_and_block*` for
any caller left, `in_crate_for_trait`, and blaze-items' `inherent_impls_in`
(`index.rs:651`, `InherentImpls::for_crate` whole), whose callers have only blaze-items'
one-way hash of the self type, not the type: a per-type read needs the pages' slots found by
that hash (the per-item form below, keyed by blaze-items' key through a hook as `GoalFacts`
is) or facts that carry the type.

**The input** is the crate's slot (`ImplTablesSlot`, `Tables { pages, mode }`), given in
`frozen::set`'s write; queries read only the slot.

**Checks** (hir-ty `method_resolution::frozen::pages::tests`):
`a_crate_read_from_impl_pages_answers_as_the_one_that_found_its_impls` with
`the_oracle_catches_rows_written_with_no_slot`;
`a_crate_read_from_impl_pages_answers_alike_under_edits_elsewhere`;
`the_same_impl_pages_from_two_databases` with `the_impl_pages_of_another_source_are_other_bytes`;
`a_previous_or_foreign_impl_page_layout_is_refused_by_name`;
`an_impl_lookup_reads_a_bounded_number_of_pages` (40 and 4,000: at most six pages) with
`a_whole_impl_table_reads_more_pages_than_a_row` and
`the_oracle_catches_an_absent_row_found_by_a_scan`. Still passing: hir-ty-mir `tests::frozen`
(Check and Verify over the pages), hir-ty `delta::tests::tables`.

**Paused 2026-11-12 (rule 66).** Front-end loading is being redesigned as one route
(`docs/v2/plan/design/end-state.md`); this lane adds no piece until that assigns it. What is
written (the pages, the narrow readers, the reader ports) is uncompiled and its compile
errors are this lane's; what follows is the design it had, for that spec to take or replace.

**Rule 65: the record is still keyed a crate (`frozen::key`), which is not allowed; the
per-item form, approved 2026-11-12, not built.** An entry (a row, a trait's blanket set, an
inherent bucket, a name's items) is a set of impls under one index (`delta::key::ImplKey`:
trait and head, or head), so its content is its members' identities and nothing of their
headers. Its key: the entry kind, the index's identity (`def_identity`, `head_identity`) and
the sorted `impl_identity` of each member (and `derive_identity` of each derive): what
`delta::read::impl_row_hash`, `blanket_hash` and the slice hashes hash today. An edit of a
body, a where clause or generics that leaves (trait, head) alone re-makes no entry; an impl
moved to another head re-makes two rows. Pieces, in order: (1) `segments` gains a stable
naming mode (a module by the names on its path, a file by owner crate and path with no
content id and no files-table index), so an entry's bytes do not move when another module or
file does; (2) entries kept and found through records-items' `hir_def::nameres::frozen::entries`
(`entry`, `keep`, `Memory` for tests; drafted at /tmp/records-items/entries.rs), under one
function pair here taking the crate's key and the entry's key, so storegc's form (a sealed
set for a crate that does not change under its key, a live set for an edited one) changes
underneath; (3) the per-crate index is today's four slot tables with a slot holding the
entry's key, plus a fifth from a self type to the traits with a row; found by the crate's key
as a first check only, re-made from hashes when it moves; (4) a reader records the index it
read with the entry's key or "absent" (cutoff's read parts; `report_stored_read` is a flag
only); (5) the test by counts: N rows, one impl moved, N-2 entries found and 2 made, a
where-clause edit makes none, and the control keyed by the crate misses all N. Notes:
/tmp/records-impls/next.md.

**Not done.** An in-place edit of a crate read from pages under random edits (one in-place
edit is `delta::tests::tables`'s); the `seal_group_whole` test that each reader's row is
marked stored; the readers listed above; the head reads the record's whole files table, which
checks the content of every file the crate's impls are in (one read a crate, not a lookup).

**State and what is left (records-items, 2026-11-12, at the end of its context; rule 65).**
The item pages were read per item but KEYED per crate (`items_key(image key)`): any change of
the crate missed every entry. Rebuilt on this form (approved; the pages deleted 2026-11-13):

1. *A file index*, keyed by H(file content id, edition): ast id to `ItemSource` for each
   item, variant and assoc item, found without parsing an unchanged file and made by the one
   parse of an edited one, from the item tree's per-item rows (`item_tree/items.rs`).
   `ItemSource` is the one per-item source hash, agreed with records-bodies: `attrs` (the
   item's attribute tokens), `header` (the item's own tree with every body cut), `body` (the
   tree hash of its OWN body node, not `BodySyntax`'s anchor), all position-free. An item a
   macro wrote is keyed on inputs, never the expansion: the outermost call's tokens, the
   `header` of each macro definition expanded on the way, a proc macro by its dylib identity.
2. *Entries under their own keys*, through `nameres/frozen/entries.rs` (written, not
   compiled: `Entries { get, put }` with a packing hint `set`, `entry`, `keep`, the singleton
   input `EntryStore`, a counting `Memory` for tests; records-impls and the def-map pages use
   it too; storegc decides the storage form behind it). Attribute flags: H(`attrs`, the cfg
   atoms they name). Assoc items of an impl or trait: H(`header`, cfg atoms, per macro call at
   assoc position its input key and the fact it resolved by, absent included). An include:
   one entry a call.
3. *The per-crate table* (item `Key` to entry key): an index, found by the crate's key as a
   quick check and made again without running any entry's work.

An edit of one item of N re-makes its file's index, its flags entry only when its attributes
changed, its impl's assoc entry only when a header inside the impl changed, nothing for a body
edit. The count test to write: edit one of N impls, N-1 assoc entries hit and the collector
does not run for them; control keyed by `items_key` misses all N.

Order for the lane that continues (its notes: `/tmp/records-items/next.md`): the tokens of a
`macro_rules!`/`macro` definition and the input keys of expansions (`decl_macro_expander`;
entry keys depend on them); the file index; the three entry kinds moved onto `entries`; the
index; then attribute values, stability, field and generic-parameter flags; blaze-items' item
key and macro token forms; positions as their own part (with cutoff-1112); lint levels; the
crate-wide tables; the signature leftovers; a test that every item of std's and bstd's crates
encodes (blaze-cargo or blaze-lower: hir-def has no std fixture); `items_key` in
`deps::front_keys` when it exists. Open with records-bodies: a body's spans are relative to
`BodySyntax`'s anchor, which is the whole `impl` for some methods, so a sibling's edit moves
them though `body` is equal; positions as a part of their own is the way out.

## A second kind of target, and where a restart's time goes (2026-11-11, lane share-front-1111)

Read from the profile of a hello-world on an 18-core Mac (`query_counts`, `misses`,
`key_parts`, the spans), then from the code. Nothing of this lane was compiled or run by it.

**`blaze test` after `blaze run` rebuilt the standard library: fixed.** A test in libtest's
harness asks std for `test` too (`std_key`, `crates/blaze-units/src/load/units.rs`), and the
directory of every std unit's outputs and `OUT_DIR` was named by the whole `StdKey`, the crates
asked among it. So all 31 std units missed, 27 of them for their `plan` alone (the outputs'
paths), and each crate above a build script (all but `core`) was another crate to the front end
(its `OUT_DIR`), with other content IDs up to `std`. The directory is now
`StdKey::placement`, the key without the crates asked; a unit's own paths already carry cargo's
hash of what it is made from. Tests: `a_test_build_plans_the_standard_library_the_program_build_did`
(blaze-units), `a_dependency_is_keyed_the_same_whatever_kind_of_target_uses_it` (blaze-lower),
each with its control.

**What is left of it (F1): the front end is one database a plan.** `unkept`
(`crates/blaze-cargo/src/serve/unit.rs`) lets the kept front end go when the plan's units are
not equal, and `blaze_lower::lower` makes a new `Db`. With the fix above the second build then
costs what a restart does (below), not a cold build; it does not yet cost only the crate under
test. The design: a checkout keeps one database, and a plan is lowered into it.
`blaze_lower::Lowered` keeps across plans what `lower_writes` now makes per plan:

| Kept across plans | So that |
|---|---|
| each load's source root ID, by the load's directory, with one allocator for package roots and doctests' (`other` no longer the count of sets) | a file keeps its `FileId` and its root |
| `made`: every crate by its root file with what it was made of, its `workspace` compared by content and its runs by their classes' unit IDs (never unit indexes, which are a plan's) | a unit whose crate inputs are equal gets the same `Crate`, and its memos stand |
| `applied` by run class unit ID; the proc macros given | a crate kept is not given `NotYetBuilt` or its base cfg again |
| the session's keys (`deps::Session`) | a kept crate is not asked of the store again |

`all_crates` is the plan's crates; a crate of an earlier plan stays, unread, until the database
holds more of them than a bound and is let go by name. Two things below it must change first, or
the kept database buys little: a crate graph change (`Written::crates`) runs every crate of the
name-resolution and impls maintainers from empty (`unreached`, `crates/hir-def/src/nameres/delta.rs`;
R8 is the same cause for a serve), so a plan that adds four crates must name them; and
`const_macros`, doctests' crates and macro hosts are made per plan and need the same find-or-make.
Its check: lower a fixture as a program, then as its tests, into one front end, and count the
runs of a dependency's rows: none (the engine's `Counts::executed` by crate).

**Where a restart's 6.7 s goes** (same binary's state, everything built; build 175 of the
profile). Before the daemon listens, 2.6 s: the profile history's two `profile fsync`s (0.72 s),
`restore blaze why's ledger` (0.39 s, `crates/blaze-cargo/src/serve/why/file.rs`), `daemon
start` itself 0.19 s. Then the request, 7.8 s:

| Span | Time | What it is |
|---|---|---|
| the watches, `std library`, `plan` | 0.45 s | |
| `lower workspace` > `maintain` (`the build lowered`) | 3.83 s | the lowering's batch ends in a pass over every crate from its source: name resolution 2.95 s (`core` 1.39 s, then `std` 1.06 s, a layer at a time), impls 0.88 s. No image is in the database yet: `deps::serve` runs after `blaze_lower::lower` returns (`Steps::lower`), so each crate an image is then found for was resolved for nothing |
| `maintain` (`the dependencies' images served`) | 0.90 s | the five crates no build script reaches |
| `maintain` x 3, after std's and compiler_builtins' runs are applied and the other 23 crates are served | 0.98 s | every crate again each time (`note_crates_changed`, R8) |
| the walk, the link, `store kept front end` | 0.5 s | |

777,858 queries ran, none of them inference or MIR: the item store answered those. So a
restart recomputes exactly what the store already holds as records (the def maps), because
the order asks the store last.

**F2: no crate is resolved before the store was asked for it.** The lowering's batch takes the
images of every crate whose key is known, and the std scripts' runs (blaze's own ports,
`blaze_units::stdlib::scripts`, pure functions of the plan) are applied in it, so every crate of
std is keyed there. Two things stand in the way and are the work: `withdraw_moved`
(`crates/hir-def/src/deps/serve.rs`) takes away any image served in a batch that also wrote its
crate's files, by design, so a serve has to say which crates it keyed after the batch's file
writes; and a crate whose key waits on a run not yet applied has to be left unmaintained (a
third state beside served and resolved) until its serve says hit or miss, with a reader of it
before then an error naming the crate. The budget: the lowering's pass after a restart is the
decode of the served maps (the 0.9 s pass, less R8), and no `nameres crate` span is of a crate
with an image.

**F3: the no-op after a restart is the answer said again.** A running daemon answers a no-op
from `noop::LastAnswer` in 20 to 50 ms with no plan and no lowering; a restart loses it (memory
only), and its safety rests on watches that vouch without a gap, which a restart breaks. Kept in
the image, it is said again only after each thing a watch vouched for is checked once at the
start: the view's root (the journal), each output's and each outside path's stat against what
the answer recorded. That is durable state deciding a build, so it lands with its simulation
(a crash and an edit between the stop and the start, the oracle that no answer is said over a
changed input), not before.

## A new project in a warm daemon (2026-11-12, lane newproj-1111; D21)

The first `blaze run` of a brand-new hello-world, in a daemon that has just built another
hello-world, should cost its own `main`. Measured on a Linux arm64 ix VM (32 cores; base
e59419ab97, the lane's 37495ec94d; one fixed `env -i` environment, a fresh home a sample,
outputs and exit status checked, the two binaries interleaved round by round, n = 6;
`/tmp/newproj-1111/measure.py` and `abab.py` on the Mac, not in the tree). A dev program is
walked and linked whole on this VM, where a Mac's takes std from the dependency image, so the
codegen rows below are Linux's: **macOS was not run.**

| | before | after | |
|---|---|---|---|
| first run of the new project | 13.85 s (12.9 to 19.3) | 13.70 s (13.5 to 14.3) | unchanged |
| its second run | 1.07 s (0.96 to 1.12) | 0.67 s (0.64 to 0.75) | the fold, below |
| its third run | 20 ms | 22 ms | the no-op lane |
| an edit and run of the first project afterwards | 12.96 s (12.3 to 13.9) | 0.79 s (0.76 to 0.99) | its front end is kept |
| unit records missed by the new project's first build | 27 | 2 | its own unit and its run |
| the daemon's CPU over that build, user + system | 48.6 + 34.1 s | 39.4 + 4.3 s | one matched pair |

The edit loop of one hello-world, the two binaries interleaved (5 rounds of 10 edits,
medians): a no-op run 18.0 to 17.0 ms, a body edit and run 78.8 to 68.3 ms (66.0 to 59.7 ms
from the fifth edit on), a comment edit and run 22.6 to 20.8 ms. The first body edit after
the builds is 761 and 752 ms at the median; over two runs of the script it took more than a
second once in ten before and three times in ten after, which this lane did not explain.

**Where the first run's 14 s go** (build 7 of a matched pair's profiles; `spans`,
`query_counts`, `misses`, `key_parts`):

| Stage | Wall | What is computed again that the other project's build computed |
|---|---|---|
| `lower workspace` | 2.6 s | a database a checkout: `maintain` (`the build lowered`) 1.9 to 2.1 s resolves all 26 crates of std from source (nameres 1.4 to 1.5 s, impls 0.5 to 0.6 s) before any image is asked for (F2), then `the dependencies' images served` 0.3 to 0.6 s |
| `unit names`, `unit check setup` of std's 26 units | 2.3 s, beside the rest | **fixed**: every unit of std missed its record (`misses`: 26 of `unit`, cause `first`); `key_part_diff` named one part, `the directory its crate's proc macros run in` |
| `admission` of `program (Codegen)` | 1.2 to 1.3 s, then 0.3 s | a program never measured asks the whole budget (granted 101.8 GB, peak 1.2 GB) and waits to run alone; **fixed**: it no longer lets go of the other checkout's kept front end while it waits |
| `unit codegen` | 7.0 to 8.8 s | `codegen walk`: `described 14202, verified 0, reached 12043` (5.5 s): every function the program reaches is described again, `mir optimize` 5,279 times, `Inline` 3,735, `SpendFacts unwind` 3,192, since a describe lives beside its database; one template is compiled (`main`), every other function's code is a store hit by a key that needs its describe first (R7; tiers.md) |
| `unit link` | 0.6 s | the first link of the program: 12,043 functions, 20.9 MB written whole |

2.13 to 2.37 million queries run (`inner_items` 180,422, `AttrFlags::query` 100,904,
`item_key` 96,813, `Body::of` 83,407, `monomorphized_runtime_mir_body_query` 60,355,
`summary_in` 58,602): none of them is new work; all of it is the other project's, in a
database that project's checkout owns.

**What was wrong, and is fixed.**

- *A crate of std named the workspace that asked for it.* The library's packages are members
  of the library's own workspace, and `crates::proc_macro_cwd` gave each the asking
  workspace's root, which is in the crate's content ID; so every std crate, its identity and
  each of its units' record keys moved with the project. They now run in the library's root.
  The workspace's root was also found from the first member in the plan's order, which is a
  package of std whenever the checkout's path sorts after the library's (`/tmp/..` against
  `/rustc/..`): there std kept one key by accident and the workspace was given the library's
  root, its `const-eval` budget and trusted list. `crates::root` now takes a member that is
  not the library's. A miss now names the setting that moved
  (`base_db::Crate::settings_parts`).
- *The second run folded the first's edges.* 1.49 million runs' edges were folded into the
  reverse index at the next build's first write (`reverse edges fold` 0.49 to 0.65 s of the
  second run); `Storage::pack_reverse`, called after the answer, folds first, and storing the
  kept front end calls it.
- *A second project took the first one's front end.* `Plane::pick` counted a task that asks
  the whole ceiling short of everything the process holds besides its grants, and the driver
  let go of every kept front end for it, though no amount let go starts such a task.

**What is left, which is all of the first run.** Sharing the front end (one database serving
both checkouts: F1 above, with the two changes below it), or making a new database cheap
(F2, then a describe as a record, R7, and optimized code under a trace, tiers.md). Neither
was built by this lane; the target (under 100 ms) stands unmet. The case
`a_second_project_reuses_the_standard_library` (docs/harness.md, `project NAME`) holds the
second project to one record missed and two functions compiled, and lists its 2.3 million
queries as known, so the bound is there to tighten.

## A restore that cannot be trusted (2026-11-11)

On 2026-11-10 a daemon restored a bad `<state>/db` and failed builds of unrelated crates with
"asked for the key Id(..), an ID this database never gave" until the directory was moved aside
by hand. What holds now:

- **The image's references are checked as it opens** (`blaze_query::image::ImageMap::check_references`,
  under the span `check the image's references`): every table slot names a memo the image
  holds and the table names as many keys as the memos have; every memo's edges, value and key
  bytes and every input's path are inside their sections. Bytes that pass their CRC and break
  this are `DbError::Dangling`, naming the section and the record, at the restore and when a
  daemon settles onto the image it wrote, never at the first request that reads the record.
  The cost is one read of the table and the memo records (8 bytes a slot, 192 a memo), which
  also moves a failed CRC of those two sections from a failed request to a refused start; the
  edges and the values stay checked as they are read. Test:
  `a_restored_image_names_only_what_it_holds`, with its control
  `the_oracle_catches_a_table_slot_naming_a_memo_the_image_lacks` (`crates/blaze-query/src/tests.rs`).
- **A restore that fails a check never serves and never starts empty.** The start stops with
  `the daemon image /<file>: <fault>; nothing of it is used and the daemon does not start`,
  and points at `blaze doctor`. The database starts empty only by a rule, each said in the
  start's log: no image, another blaze's layout, a section of another schema.
- **A key no database gave names the restore** (`blaze_query::db::NeverGiven::restored`): when
  the process restored from an image, the report says its file, layout and hash and that the
  restore is suspect; when it restored nothing, it says no restored file holds the key.
- **`blaze doctor [--json]`** (`crates/blaze-cargo/src/cli/doctor.rs`) checks the state
  directory without a daemon: the image's layout and spine, every group's CRC, the database's
  references as above, and that the profile history's manifest reads. Each part is `ok`,
  `absent`, `unused` (whole, another blaze's: no fault) or `FAULT`. For a fault it prints the
  command that sets the part aside whole (`blaze stop && mv <state>/db <state>/db.suspect-<time>`);
  it moves and deletes nothing itself, and exits 101. It reads files, so it is not a request
  of the daemon's API: it is for when no daemon starts.

## Persisted state that is hard to get wrong (2026-11-11, lane restart-1111)

What is written here is built unless it names a ledger row (R1 to R9 in
[objectives.md](../objectives.md)); nothing of this lane was compiled or run by it.

### What is persisted, and what ties it

| File | Holds | Its commit | Tied to the root by |
|---|---|---|---|
| `<state>/db/image` | the query database's memos, the VFS tree, the plane's history, the symbols, the library syntax trees | one rename (`blaze_image::place`), after the clone's sync | it is the root: its hash covers the spine, the spine every group's CRC and each section's schema |
| `<state>/journal` | VFS changes since the image | appends, cut at the last whole record | names the image's hash; a journal of another image stops the start (`VfsFault::Unnamed`) |
| `<state>/store`, the shared std store | records by content key: dependency images, checked bodies, the item store, goals, templates, outputs | the store's own (blaze-cas) | nothing: a record is found by a key that covers its inputs and is checked against it, so one that is missing or fails is a miss, never a wrong value |
| `<state>/db/cold/<n>` | memo values moved out of memory | none | nothing, and no start reads one: garbage after a restart (R3: removed at start) |
| `<home>/v2/profile` | the profile history | `manifest.json` under its lock | nothing: it decides no build |

So there are two commit points a build's state depends on, the image's rename and the
store's commit, and they cannot disagree in a way a later start accepts: the image holds
no key of a store record that the store may not have (a store miss is recomputed), and the
journal names its image.

### The six constraints

1. **One commit point.** Held by the image for everything addressed by ID. The store is
   content-addressed and is reached only by keys computed from inputs, so it needs no
   root. Not held: cold packs are written and never reclaimed across a restart (R3).
2. **No process-local ID crosses the process boundary.** By table:
   - the query database's memos: content IDs (`QueryKey`), never a dense ID;
   - symbols: IDs of the image's own symbol table, restored first and checked whole; a
     database section is used only when `intern::restored_from()` is its image's hash;
   - dependency images, checked bodies, the item store, signatures: every ID goes through
     a relocation table (`hir_ty::check_cache::ids`, `hir_def::hir::persist::relocating`)
     to a portable form, and is made again by interning; a table index is checked for
     bound and kind at decode (`IdRefused`);
   - the front end's dense database (`blaze_query::db`): not persisted at all.
   `Id::decode` is the one way from a word to an `Id`; it now states its contract and is
   `#[blaze::nopanic]`. Not held structurally: the relocating scope is a thread-local, so a
   record written outside it holds this process's IDs and nothing in the type says which
   it is (R4: the encoder takes the table as an argument).
3. **Every record self-verifies.** The image: a CRC32C per 64 KiB group, checked on the
   group's first read (about 3 µs, then one relaxed load a read), and the reference check
   at open. The store: by its key (not re-read by this lane).
4. **Persisted state is only a cache.** A start uses a part or says why not
   (`Started`); a damaged image stops the start naming the file. Granular recompute of
   one damaged group, in place of the stop, is not built (R3).
5. **The write path is a type.** `blaze_image::Txn` is `#[blaze::linear]`: an image begun
   ends in `commit` (to the placer) or `abort`, or the code does not compile; and
   `Placer::place` is the crate's own, so no code writes the image's file without one.
   The placer stays the `#![blaze::sans_io]` core the crash simulation drives.
6. **Evidence.** (a) `blaze-image/src/place/tests.rs` (every effect of a placing, torn
   writes) and `blaze-query/src/tests/restore.rs` (the database across images, power cuts
   and starts); (b) the last step of that test changes a byte of the file and holds every
   answer to the model or a typed refusal, and `a_corrupt_group_fails_the_keys_in_it`;
   (c) restored, warm and fresh databases answer alike in the same test; (d) the kill
   loop on a real build is R3, not written.

Costs: the transaction adds nothing to a commit. An open reads the spine (128 bytes, 64 a
section, 4 a group) and hashes it once, then the table (8 bytes a slot) and the memo
records (192 a memo): for 100,000 memos about 21 MiB read and CRC'd, some tens of
milliseconds, measured by the span `check the image's references`.

### Language features used, and what waits

| Feature | Used here | Waits on |
|---|---|---|
| `#[blaze::linear]`, `#[blaze::consumes]` | `blaze_image::Txn` | |
| `#[blaze::nopanic]`, `#[blaze::noalloc]` | `blaze_query::db::Id::new`, `Id::decode` | R4: the open path (`Image::open`, `ImageMap::open`, `check_references`, `open_image`). `nopanic` refuses every global allocation (`handle_alloc_error`) and every index, so the path's lists go to a frame and its `bytes[a..b]` to `get` first |
| native `ensures` (`blaze_verify`) | `Id::decode` | R5: `restore(persist(x)) == x` on `blaze_format::Persist`, which needs a clause that names a value behind a reference; contracts.md says today's lowering takes the return and by-value arguments only |
| `#![blaze::sans_io]` | the placer (already) | |
| reflection (`for const`) for layouts and schema hashes | none | R5: the generic `Persist` over `for const` is planned in memory-and-data and not in this tree; the hand-written `SchemaHash`es stay until it is |
| `fn_info(&f).code_id` as a memo's producer | none | R6: const eval does not answer `fn_info` yet (comptime.md, "Wired once hir-ty-identity compiles"); `Query::PRODUCER` is a string versioned by hand. `the_oracle_catches_code_changed_under_one_producer` shows what that allows |

Version skew is tested at the level the tree has: an image of another layout or schema
starts empty by rule, and a memo of another producer is never read
(`a_memo_made_by_other_code_is_never_read_as_this_codes`).

### What an edit needs (A: a restart, then a body edit)

Measured: 53 to 57 s and 375 s of CPU, against about 1 s warm. Read: nothing of the front
end is in the image, by design (its IDs are a process's). What a restart can read is in
the store, by content key, lazily: dependency def maps and signatures, each dependency's
checked bodies (`check_cache`), each body's inference, MIR and borrow check verdict (the
item store), goals, templates. What it cannot read:

| Recomputed | Count on ripgrep | Why | Row |
|---|---|---|---|
| inference | 21,624 ran | each is a miss of both stores (`infer_body_query` asks them first). Which miss is not known: the event `item store flushed` now says hits, runs and refusals by step, with the read that kept most out | R1 |
| describes | 86,702 | a describe is a memo of the dense database only; the kept walk names its IDs | R7 |
| the walk, lists, link base | whole | kept in memory beside the database | R7 |
| the workspace's lowering | 4.7 s | `KeptLowered` is memory only | R1 |

The design: the store stays the one place, and a describe becomes a record there
(`Step::Describe` beside the item store's three: base key from the instance's identity,
the requester and `Facing`; shapes of the facts it read; the answer with its references in
portable form). A walk after a restart is then 86,702 lookups on all cores, not 130 s of
describing, and the walk itself is made again from them: nothing ID-bearing is written.
It is not built here because a describe's reads have to be classified as facts
(`BodyFacts::classify`) and that cannot be done without running one; built blind it would
refuse most describes or, worse, cover too little (keys-cover-every-input).

The budget (R1): restart to the end of the first body edit at most 2 s over a warm edit
on ripgrep, reading at most 64 MiB; bounded by the instances the roots reach, one lookup
each. The no-op after a restart is 13 to 14 s today and should be the start's spans plus
one plan verify; which span holds it is not known from reading (`daemon start: *`,
`lower`, the plan's verify are the candidates).

### B: the first edit after a cold build

5.7 s against 0.95 s. Read (incremental-walk.md has the table): the cold build let go of
the edited crate's inference by name and of every MIR; the check's memos held, so the only
readers were the body lints, on one thread. Built: a cold build holds the inference of the
last `EDIT_CRATES` workspace crates it finished. Left (R2): their MIR still goes, and the
lints still read on one thread.

### C: a daemon that starts on bad state

Not found by reading. What reading rules out: a dense `Id` cannot come from the image or
a store record without passing `Id::new` (which refuses 0), so `Id(0)` was read from zeroed
memory of the running process. Ranked:

1. A value of the dense database read after the release let it go. The restored part that
   changes this is the plane's history, which changes how many tasks run at once and in
   what order; the first `Id(0)` (2026-11-09) came with no restore.
2. A served dependency image with zero bytes inside a part that passes its key.
3. The method-resolution stall is separate: each `deps::serve` that fetches a crate raises
   `written.crates`, and every such write warms every crate again (R8). A fresh store
   fetches nothing, so it never happens there. The stop's guard then ended the daemon
   because verified memos and loop items were not counted as work (fixed).

### Which part a fault follows

`BLAZE_RESTORE_WITHOUT=database,history,trees` starts a daemon that leaves those parts of
a good image unused, each said in the start's log. On a kept bad state: all three (the
VFS alone is restored), then one at a time. The part whose absence makes the build clean
holds the fault; `history` alone clean is candidate 1.

## Sealed crates (2026-11-12, lane sealed-1112; D21, D29)

What is built is said as built; the rest is the design the next commits follow.

**What "sealed" is.** A crate is *sealed* while all three hold, whatever its source (the
toolchain's, a registry or git package, a path dependency, a workspace member):

1. *Quiet.* None of its inputs was written for a while: the files of its source root, its
   crate data, cfg, env, proc macros and the build-script runs applied to it. A library crate
   (origin not `Local`) is quiet from the moment it is lowered. A local crate is quiet once
   two builds in a row of its checkout ended without a write to any of them (the same two the
   reader shed waits for, `blaze_query::db::frontier`); the count is the checkout's, kept with
   its front end, never a clock.
2. *Keyed by content.* Its key is `hir_def::deps::keys`' (source root file contents by path
   under the package, crate inputs, the front end's code identity; no absolute path), with
   one change still to make: each dependency enters as the parts of it this crate read, by
   value (cutoff-1112's rule, store.md, "Early cutoff between crates": per item,
   position-free), not as its key. One key rule, one store (`blaze_cas`), the same key for a unit record's front-end
   part.
3. *Recorded.* Its records are in the store under that key: the interface (def map, lang
   items, signatures: today's image parts; impls by self type and trait are to be added), and
   by body key its bodies, inference and MIR (the item store).

A sealed crate is, to the engine, one seal group (below): its rows are proved by one check of
what they read from outside the crate. **What a dependent's front end gets** is that interface record
read in place (names to items, signatures, impls), and bodies by body key when it asks
(generic and `#[inline]` functions, constants): a read of a body is a read of that body's
record, so a body edit below reaches the dependents that read that body and no other.

**What unseals one.** A write to any of its inputs, at once, in that write: the write finds
the crate from the input (file to source root to crates: `base_db::source_root_crates`; a
crate's own fields name it), with no walk of rows, as `hir_def::deps::withdraw_moved` does
today for a served crate's image. Only that crate is unsealed. Its dependents stay sealed
while the parts of it they read are unchanged after the edit (a body edit), and the seal of
each dependent that read a changed part breaks (a signature edit): found by that dependent's
one check, of the stamps of the rows it read, the first time a row of it is asked or by the
walk. The same set is cutoff-1112's parts, there by value (`Storage::seal_group_imports`).

**What an unseal costs** (the first edit of a crate, with a person waiting): the edited file
is parsed; the crate's def map is decoded from its interface record (hello-world's `core`,
the largest, decodes in under 0.9 s with every other served crate today, restore.md above; a
workspace crate is a fraction of it); its rows are made again as they are asked, each body's
inference and MIR found by body key in the item store. Bound: the crate's own items, never
its dependencies' or its dependents'. Not measured yet for a workspace crate: nothing seals
one today.

**What sealing again costs.** After the answer, never on a request's path: the quiet crate's
records are written if its key is new (the `store kept front end` task does this for
libraries today), its rows' kind is raised by one pass over its rows, and its heap values are
let go. A request that arrives stops it.

**The engine: seal groups** (written 2026-11-12 by lane sealgroups-1112 in
`crates/blaze-query/src/db/seal.rs`, whose module text has the invariant and its argument; not
yet compiled or run: the round runner's results decide). Three kinds for the whole database
cannot say "only it", so a crate is a *seal group*:

- *Which group.* The host makes a group a crate (`Storage::seal_group`, with the groups it
  depends on, only ever added to) and gives each input its group (`plumbing::input_seal_group`):
  one word a node, which is the index from a written input to its group. A row's group is
  worked out from its edges as its run ends, beside its summary: the group among those it read
  that depends on the others, none when no one of them does. One bit of the input summary
  (`LOOSE`) is on every input of no group and every row no one group covers, and so on every
  row that reads one at any depth: a row without it reads only its own group's inputs and
  those of the groups below.
- *When sealed.* The pass a host calls at a build's end (`Storage::seal_quiet_groups`, after the
  answer and the settle) seals a library group at the first pass that finds rows of it, and a
  local group at the pass that ends the second build in a row with no write to its inputs and
  no broken seal (`SealKind`); the count is of passes, never a clock.
- *One check a group.* A sealed group keeps what its rows read from outside it (its imports,
  sorted by source group), the view it was sealed in, and the OR of its loose imports'
  summaries. A write reads the field's word and opens that group and the groups declared to
  open with it (`seal_group_opens_with`: a package's files and the crates made of them); one
  loop over the groups makes suspect each held group that depends on it or reads the field
  loosely. A suspect group is checked once, the first time a row of it is asked or by the walk:
  imports behind another seal that holds are passed over a run at a time, the rest are brought
  up to date and their stamps compared with the seal's view. Held: each row of it is proved by
  one load. Not: the seal is broken, and (rule
  granular-caching: a seal is the quick first check, never the grain of what is redone) the
  walk indexes the group's rows from their own edges and verifies only the rows that read an
  import that moved; every other row of the group is proved by one load once the walk ends.
  The next write opens the group. So a dependent stays sealed while
  nothing it reads of the dependency changed; the engine compares the stamps of the rows read,
  it defines no digest, and a host that has a dependent read a dependency through its
  interface record gets cutoff-1112's digest as that one import.
- *No reverse edges.* A walk never enters a sealed group (`Plan::suspects`, `reach`). Once
  the readers behind seals are half the index, the pass makes the index again without them
  (`shed_sealed_readers`); when a group opens they are made from its own rows.
- *What an unseal costs.* The opened group's rows and their edges (to index them), on the
  write's path, under the span `seal group open`; nothing of another group. A broken seal
  costs its group's edges once (an index, no row verified), its imports' stamps, and the
  verification of the k rows that read what moved, of its N (test
  `a_broken_seal_re_verifies_only_the_rows_that_read_what_moved`: 3 of 100 and 3 of 2,000). A sealing costs the
  group's rows not yet behind the seal and their edges, after the answer.
- *The census* (`Storage::seal_census`, and in `durability_census`'s text): groups sealed,
  rows behind seals, rows by group and state, checks, seals broken, groups opened.

What is not built: the hosts' calls (no crate makes a group yet, so nothing is sealed in a
real build); letting go of a sealed row's forward edges (a row without edges must run again
when its group opens: right for a library, wrong for a local crate); the provider a sealed
row's value is fetched through (records-1112b's functions, by value, where a let-go value is
asked: `function::claimed`'s `Cause::Evicted` run).

Memory a row behind a seal, as built: its header 32 bytes and its value's place, its node 32,
its last run 8, its group word 4, its place in its group's list 4, its forward edges 4 each
(about 12 a row on hello-world): about 130 bytes and the value. With the edges let go, 80.
1.8 million library rows are then 140 MiB, three times the 48 MiB budget: the budget needs a
sealed group's rows not to be rows at all (the group one leaf and a record read through the
pool, a row made only for a key that is asked), which is records-1112b's half.

Tests (`db/tests/sealed.rs`, each with its control): `an_edit_under_sealed_dependents_stays_in_its_group_at_two_sizes`,
`a_write_to_a_sealed_groups_input_opens_that_group_and_no_other`,
`an_interface_change_breaks_the_seals_of_the_dependents_that_read_it`,
`an_equal_set_of_an_input_every_crate_reads_unseals_nothing`,
`a_plan_that_adds_a_crate_unseals_nothing`, `a_local_group_is_sealed_after_two_quiet_builds`,
and the refinement `a_sealed_database_answers_as_an_unsealed_one_under_random_writes`.

**Where this meets the other lanes.** firstedit-1112 changes how a walk uses what changed
(readers that saw the change are passed over); this lane changes which rows a walk can reach
at all. They meet at `Plan::suspects` and `frontier::clean`: a row of a sealed group is never
a suspect, and `clean` must answer for it from its group's view, not from the walk. No file
of the two lanes overlaps until the seal groups are built (`row.rs`, `function.rs`,
`storage.rs` then; `frontier.rs` one line). content-keys-1112 owns the interface digest a
unit's key takes for a dependency; the leaf's value is that same digest, made once
(`hir_def::deps`), and its unit keys read it.

**Built (b1, first half): a library crate's rows are `HIGH`.** The census
(`Storage::durability_census`, said by a release when `BLAZE_DURABILITY_CENSUS` is set) names
the fields that held them at `MEDIUM`: not `Resolutions` and `ImplSlices` by many direct
reads (28 and 7,191 rows) but through what those few rows make: each crate's `DefMapPair`
(466,718 rows read one), `ModuleScope` (117,442), `ItemBodySyntax` (116,449), `Modules`
(65,809); and each file's source root and each root's file set (7,586 and 7,546). Library
crates are now found in tables of their own at `HIGH` (`LibraryResolutions`,
`LibraryImplSlices`), and a library root's file set and its files' roots are `HIGH`. The
numbers are in the lane's report with the binaries they are of.

**One front end a checkout (F1, above): written 2026-11-12 by lane onefront-1112, not yet
built or run (the lane writes only; the round runner's results decide).** Measured by the owner
on hello-world before it: `build`, then `test`, then `build` cost about 4.5 s at each switch
(`lower workspace` 3.6 s, `maintain` 2.7 s, 820,026 queries run, 5,087 inputs written), because
`unkept` let the front end go when the plan's units differed.

The invariant: *a request's units are a view over one crate graph a checkout; a source root, a
directory read and a crate are each made once a front end, found again by what they are by
every plan that names them, and never unmade because another plan arrived; so a plan lowered
before writes no input, and a new plan writes the crates it adds and their files alone.*

How it is held:

- `blaze_lower::Lowered::relower` lowers another plan into the kept front end: `lower_writes`
  finds or makes each thing in `Graph`, which the `Lowered` keeps (table below), and gives a
  new `Crates` (the plan's view). `Relowered::Into(Placed)` says what it made; nothing, for a
  plan seen before. `serve/unit.rs` calls it when the request's units are not the kept ones;
  `unkept` no longer compares units.
- A crate made is noted as that crate added (`base_db::Crate::create_added`,
  `base_db::add_crates`, `Written::crates_added`), not as a change of the whole graph, so name
  resolution runs the added crate and no crate that was there (`delta::unreached`), and a file
  set that changed (the twin's alias in its package's root, a new package's root) reaches a
  library crate only when it is that crate's own root (`delta::files_reach`).
- The daemon keeps a checkout's plans by request shape (`Roots::plans`, `keep_plan`, at most
  `PLANS_KEPT`, the least recently used let go), so `build`, `test`, `build` finds the first
  plan and its `Arc<Units>` again.

What a crate instance shares with its sibling (the plain crate and its `cfg(test)` twin, a
crate under two feature sets or two profiles), read from the code:

| Shared | Why |
|---|---|
| the source root, its file set and every file's text | a package's files are read once into one root (`Graph::set_of` by package id) |
| each file's parse (`EditionedFileId::parse`) | keyed by file and edition, not by crate: both instances of one edition read one tree |
| the dependencies' crates, with everything resolved, inferred and compiled of them | a dependency made of the same data, cfg, env and spelling is one `Crate` (`Made`), so `core`, `std` and every dependency whose features and profile inputs are the same are found, not made |
| the workspace data (target, toolchain, const-eval budget) | one `Arc` a triple (`Graph::workspaces`) |
| a build script's run and its `OUT_DIR` files | a run is applied by run class to each crate that depends on it (`Lowered::applied`, by the class's `OUT_DIR`) |

| Not shared | Why |
|---|---|
| the root file of a crate blaze appends a module to (a test's harness) | its own alias of the file on disk (`Db::alias`), so its text is the file's and the harness |
| the item tree as a crate sees it, the def map, and everything keyed by `Crate` above them (scopes, signatures, inference, MIR) | cfg decides which items exist; these are per crate today, with no sharing of the items both instances keep. Sharing them by content (an item whose tokens and enclosing cfgs are the same) is the item store's by body key for inference and MIR; the def map is not shared |
| the unit's code and its record | keyed by the unit |

So the first `test` after a `build` still resolves and checks the twin crate whole (bounded by
that crate, not by the workspace or std); every later switch is found.

What is not built: the no-op lane's kept answer is still one a checkout and dropped by any
walk (`Roots::answers`, with `walked` and `stills`); `Roots::sized` and `Roots::outputs` are
still one a checkout by the units' pointer (they now hit again when a plan's `Arc<Units>` is
found by `keep_plan`, but hold one plan); the build-ahead's `Checkout.last`. Three plans are
lowered anew, by name, not into the front end (`Relowered::Anew`): one with const macros (their
crates are made by `const_macros::make` into the plan, not the graph), one whose workspace
root differs, and one that makes a directory of its own inside a directory read whole (a
package nested in another that was read first).

What `blaze_lower::lower_writes` made a plan, and where each is kept now:

| Made per plan before | Kept across plans by |
|---|---|
| source root ids, `0..sets` in the plan's order of package sets, and `other` | `Graph::roots`, `set_of` (package id), `std_set`, `other`; a later plan's new packages take the next ids; a doctest example's root is taken from the same list |
| `LibraryRoots`, `LocalRoots` | made by the first plan, set again only when a plan adds a root |
| `made`: a `Crate` a distinct (root file, data, cfg, env, runs, spelling) | `Graph::made`, the same tuple; the runs are in `extra.build_script_runs` by the class's unit id, never a unit index; `Crate::create_added` only on a miss |
| `all_crates` (set to the plan's) | the union: `base_db::add_crates` appends what a plan made; a plan's own list is `Crates::units` |
| `Lowered::applied` by run class index | by `Crate` and the class's `OUT_DIR`; `class_runs` and `appended` are per job and cleared |
| `ProcMacros::give(NotYetBuilt)` for every proc-macro crate | only for a crate this plan made |
| `PrimaryCrates` (set to the plan's) | the plan's crates as the plan says; a crate the plan does not name stays as it was |
| spellings and `name_sources` | `Graph::spelled`; made again only when a crate was made |
| const macros' crates | not kept: such a plan is lowered anew (`Relowered::Anew`) |
| doctests' crates, by unit index | by the unit of the current plan that has their crate (`OfUnit::unit`, remapped by `relower`) |
| `KeptLowered::{units, session}` compared whole (`unkept`) | `units` no longer compared; the session's keys by `Crate` stay |

Its checks: `blaze-lower`'s `a_plan_lowered_again_is_found_whole_and_writes_nothing` (A, B,
A, B, A: the database's view, its maintainers' passes and each unit's crate stand) with its
control `the_control_a_plan_that_is_the_front_ends_identity_makes_its_crates_again`;
`base-db`'s `a_crate_added_beside_the_graph_names_itself_and_not_the_graph`; the revision case
`switches_of_request_are_no_ops` (build, test, build, test, run, check, release build, dev
build, and each again: after its first occurrence a request makes no unit and runs at most
2,000 engine memos) with its control `a_switch_of_request_with_the_plan_as_identity`
(`BLAZE_FRONT_END_PER_PLAN=1` puts `unkept`'s units test back; its miss is listed as known, so
it fails when the bound stops resting on this); and `harness/v2/goal_switch.py`, the owner's
sequence timed, each `test` held to having run.

## Bodies, inference and MIR as records (2026-11-12, lane records-bodies; D21, D29)

**Written, not compiled or run by this lane**: the round's results decide; nothing below is a
measurement of this lane's.

**What was stored a function, read from the code.** Two stores hold per-function results:

| Store | Key | Holds | How it is read |
|---|---|---|---|
| `hir_ty::check_cache` (a value a dependency crate) | the crate's image key and the borrow checker's mode | each body by `body_key`: its `InferenceResult` and whether its borrow check passed | the whole part pinned for the session (`ServedBodies`); the crate's whole item table decoded at the first body (`served_items`); a body by binary search of the key column |
| the item store (`hir_ty::body_store`, any crate) | `BodyFacts::identity`, the step, the schema, the solver version, then the values of the facts the step read | a body's inference, its BUILT MIR (an answer a MIR owner: body, closure, coroutine, by-move, an anonymous constant of the body), a passing borrow check | a shape and an answer a step, decoded into the query's memo |

Stored by neither: runtime MIR, optimized MIR (`mir::persist` refuses inlined tables),
promoted bodies, coroutine witnesses, held locals, the lowered body.

**What goes after a unit stores** (`release.rs` to `hir::evict::drop_crates_after_store`): the
crates' inference, and with `mir` every value tagged `Mir` of every crate (built, runtime,
optimized, monomorphized, promoted, `location_literals`, witnesses) and every constant's value.

**What a dependent's codegen then made again, a dependency body.** `instance_body` asks
`db.mir(owner, MirStage::compiled(owner, options))`: the optimized body (when the options ask)
from the runtime body (`run_passes` again) from the built body, which asks the item store.
Three reads made even a hit cost a lowering:

1. the item store's key is made from the lowered body (`identity_parts`: `Body::of`,
   `names_of`, `Body::has_lowering_errors`), so `body_query` ran before the store was asked;
2. decoding a stored MIR called `locations()`, so `location_literals`, so `Body::of`, and
   `persist::read` took its bounds from the body's store;
3. `check_cache::served_inference` called `Body::of` for `Nodes::of` before it decoded.

On a miss: `build`, so `InferenceResult::of` (let go: decoded or inferred again), so the
lowering. Each of the three stages is then a global-heap memo until the next `drop_mir`.

**Built in this piece.**

- *A crate's runtime MIR is one value read an entry at a time*
  (`hir_ty::check_cache::{pages, runtime}`). Granularity: a crate, with a table by function
  key, not a record a function by its code identity: that identity is made from the lowered
  body, so asking for such a record costs the `body_query` it exists to avoid; a crate with
  records is one no edit reaches, so the crate's key is exact there, and it is one key for
  pins and exports. The item store stays the form for a crate being edited (rule 52).
  - The container (`pages.rs`): header, buckets of four slots, entries. A key is 32 bytes of
    a content hash; two of its words choose two buckets, a third is its tag; the writer puts
    an entry in the emptier bucket in key order and doubles the buckets until all fit. A
    lookup reads at most two buckets (64 bytes each) and the entry: bounded for any crate.
  - An entry (`runtime.rs`) is one MIR owner at `MirStage::Runtime` (what `MirStage::compiled`
    gives for options that ask nothing more, and what every optimized body is made from; no
    `MirOptions` in the key). Its key: the owning item's `body_key` and which owner of the
    body (tag, anonymous constant, expression, closure kind). Its bytes: `mir::persist`'s
    words in a relocating scope, its ids, the items they name as a portable table of its own
    (a reader decodes one body's items, never the crate's), and the counts of the body's
    store, so `persist::read_in` bounds the spans without the store.
  - The value's key (`runtime::key`, `Kind::Query`): the crate's image key as filed (sources
    with the crate attributes that change MIR, cfg, features, env, target, toolchain, every
    dependency's key, the front end's code identity), the entries' schema (the MIR words'
    version, the type table's, the ids', `frozen::ENCODING`, the key domains) and the
    container's layout. The runtime passes read types, layouts and drop glue of the crate
    and what it loads, all under the image key. Not in it: lint levels, paths, options.
  - Written from the lowering task's value: the runtime queries note each library owner they
    build (`runtime::made`, when the database records: `runtime::record`); `runtime::encode`
    asks each noted owner's entry, a memo of its bytes the engine keeps true (an owner whose
    inputs moved is made again there, never written as it was).
  - Read in `providers::compute` before anything is built (`runtime::served`): a crate given
    its value (`runtime::serve`, a slot of the crate, `Crate::give`) answers an owner the
    value holds from its entry and reports a stored read; the built body, the inference and
    the lowered body of that owner are never made. Off until a driver gives a value: no
    caller of `serve` exists yet, and with none the old path is the only path.
  - A body that calls a location macro, or has no word in the codec, has no entry
    (`runtime::Left`) and is built by the reader as before.
- *A served inference lowers no body*: the bodies part keeps each body's node counts
  (`BodyRoot` 2), and `served_inference` bounds the record by them.

**Spans.** A `MirSpan` is an `ExprId`, `PatId` or `BindingId` of the body's store, never a
position, so an entry does not change when a line is added elsewhere in its file. What is
missing is a span's position without lowering the body (`hir-ty-codegen/places.rs` reads
`Body::relative`): proposed to cutoff-1112 and records-items as a segment of its own a body
(the body's key and position, and the range of each node relative to the body's start), not
positions inside MIR. A location literal is already a hole in the words (slot 5: the
literal's expression and its type); filling it from that segment and the build's name of the
file, as named inputs, is what lets a located body have an entry.

**What is left, in order.**

1. *The driver's calls* (other lanes' files; lines sent): `record` once a front end;
   `encode` and a put under `runtime::key(image key)` where the bodies part is stored
   (blaze-diagnostics `cache.rs`, `deps::store`); `serve` with a `PageSource` over
   `blaze_cas::Reader::part` in `unit/deps.rs` `images`, in the serve's batch; the key form to
   storegc and packs; a telemetry row for the slot. Then the switch is the value's presence.
2. *Per entry: promoted bodies and the held-locals fact.* `promoted_mir_body_query` reads the
   built body, so a served owner with promoteds still lowers it; a coroutine's layout
   (`layout.rs`) reads `held_locals_in` of the monomorphized runtime body. The fact (the held
   locals, the yields, the conflicts, each held local's generic type) is of the generic body
   only when monomorphization changes no block (monowalk-1112: `comptime::expand` and
   `fold_if_const` add and remove blocks for `for const` and `if const` bodies): such a
   coroutine keeps the monomorphized read, the rest read the fact and substitute through
   `monomorphized_ty`.
3. *The bodies part in the same container*, then the old part deleted: an entry a body
   (inference, verdict, counts) with its own items table, so nothing is pinned for the
   session, no crate-wide item table is decoded and a lookup is two buckets.
4. *The item store's key before lowering* (the crate being edited): the lookup's base key
   from the item's source content hash (records-items' body key and content) and the facts'
   values, with the body-derived identity kept in the answer and compared after a hit, so an
   unchanged function in an edited file is a hit with no `body_query`. `identity_parts` is
   blaze-item-hash's; its "hir", "entries" and "names" parts are the ones made from the body.
   With it, the answer keeps the store's counts and a located bit as an entry does.
5. *`MirBody` in the asking task's frame.* `MirBody` is a heap tree: `Arena<BasicBlock>`,
   `Arena<Local>`, `Vec` and `Box<[..]>` in statements, operands, switch targets and the
   side tables, `ArenaMap`s, `SmallVec` projections. Decoding into a frame means
   `MirBody<'db, A: Allocator>` with every nested owner over `A`. What the port touches: the
   types in `hir-ty/src/mir.rs`; `mir/persist.rs` (`Decode` takes the frame); every pass in
   `hir-ty-mir/src/mir/{lower,transform,optimize,early_drop,handles}` that builds or edits a
   body; `monomorphization.rs` (it clones); the 28 MIR queries of `providers.rs` (a frame
   value cannot be a memo: `mir_at` hands a task-scoped body, as `dispatch::body_once` does
   for instances); and each reader that holds `&'db MirBody` past its task. Mechanical once
   the type has the parameter (`blaze check --replace`), but one whole port (rule 24). Until
   then a served runtime body is still a `Mir` memo between `drop_mir`s: read again from its
   entry, never lowered again.
6. *Optimized bodies of a library crate* are made from the served runtime body each time
   they are asked after a `drop_mir`: an entry a `MirOptions` needs the inlined tables in the
   codec and the callee bodies under the key (the image key covers them for a library).

**Checks** (none run yet). hir-ty `check_cache::pages::tests`:
`every_entry_written_is_found_with_its_bytes_and_no_other_key_is` with
`the_oracle_catches_an_entry_left_out_of_its_bucket`;
`the_same_entries_in_any_order_are_the_same_bytes`;
`a_lookup_reads_a_bounded_number_of_pages` (40 and 40,000 entries) with
`reading_every_entry_reads_more_pages_than_one`;
`a_previous_or_foreign_layout_is_refused_by_name`; `two_entries_of_one_key_are_refused`.
hir-ty `check_cache::tests`: `a_record_keeps_the_node_counts_of_its_bodys_store` with
`the_oracle_catches_node_counts_that_are_not_the_bodys`. hir-ty-mir
`tests::check_cache::runtime`:
`a_runtime_entry_reads_in_another_database_as_the_body_built_there` with
`the_oracle_catches_an_entry_of_another_owner`; `the_same_runtime_value_from_two_databases`
with `the_runtime_value_of_another_source_is_other_bytes`;
`a_crate_read_from_its_value_builds_no_body_it_holds` (one MIR value an owner asked, no built
body) with `the_control_a_crate_with_no_value_builds_every_body`;
`a_served_runtime_body_is_the_one_built_from_source`;
`an_entry_that_does_not_read_is_a_panic_naming_the_crate`;
`a_runtime_value_of_another_layout_is_refused_by_name`. Not written: the refinement under
random edits elsewhere (it needs the driver's serve and withdraw), and the count on a real
build (`query_counts` of `Body::of`, `infer_body_query`, `mir_body_query` for a dependent of a
served crate: 0 for the owners the value holds).

### One form, a function: the end state (2026-11-12, after the owner's review; design, not built)

The piece above reads a function at a time but is keyed a crate at a time, so any change in a
crate misses every entry, and it leaves two forms of one idea (the crate value for quiet
crates, the item store for edited ones). The end state is one form, a function, for
dependencies and edited crates alike; `body_store` and `check_cache`'s part are deleted into
it. What stays of the piece above: the container (`pages`), the entry codec, `read_in`, the
served arm of `compute`. What changes: an entry's key and its lookup.

**What exists already, read from the code.** The item store is most of it: an answer a
function and step, found by a base key and then by the values of the facts the step read
(`record::shape_key`, `AnswerKey`), the facts classified from the step's captured reads
(`blaze_item_hash::reads`, `RULES`): `Signature`, `Fields`, `Attrs` of the items it names, the
impl indexes by trait and self type, `Crate` and `CrateRoot` facts, lane items' negative facts
(`ImplIndexKey`, `ScopeIndexKey`). Three things keep it from being the one form:

1. its base key is made from the lowered, resolved body (`identity_parts`: "hir", "entries",
   "names"), so the lookup costs the lowering;
2. name resolution's reads are `Rule::Names`: covered by that identity, never recorded;
3. an answer names a path's target as "target `i` of the body's resolutions"
   (`keys::Rel::Target`), so decoding calls `names_of`, which lowers and resolves the body.

**The entry's key, made before lowering.** The base key is the function's own content and
nothing of its crate's: the tree hash of its anchor (`hir_def::expr_store::body::BodySyntax`'s
`tree`: the item's text, position-free; an item a macro writes: the call's and the macro
definition's, below), its owner part and module chain as today (`identity::owner`, by key and
signature), the step, the MIR owner's place, the schema, the front end's code identity, and of
the crate only its settings (cfg, features, edition, target, toolchain: `crate_inputs` less
the dependencies' contents). Under it, as today, shapes list what was read and the answer is
filed under those reads' values.

**The reads an entry records, and how each is checked at a lookup.** Each is a fact whose
value is a memoized query, so a check is that row's verify, and a body nobody edited checks
by fingerprints alone:

| Read | Recorded as | Value compared | Negative form |
|---|---|---|---|
| a name looked up in a scope (`ScopeInput::name`, `legacy_macro`, a block's scope) | the scope (module key) and the name's text | the item keys it resolves to, a namespace each | the name absent from the scope: the same fact with the value "nothing", so a new item that shadows or supplies it changes the value |
| the traits in scope of a module (`ScopeInput::traits`) | the module key | the traits' keys, sorted | (a set: an added trait changes it) |
| a glob or prelude that supplied a name | the name fact of the scope it came through | as a name | as a name |
| a macro expanded in the body | the macro's item key | the hash of its definition's tokens (a proc macro: its dylib's identity, in the crate's settings) | none: found by a name fact |
| the signature, fields, attributes, generics of an item named | `Signature`, `Fields`, `Attrs` (as today) | the fact's hash | none |
| an impl looked for by trait and self type, a method candidate by name | the impl index facts (as today) | the index entry's hash | "no impl": the entry absent, as today (`ImplIndexKey`) |
| another function's inference, MIR, constant value (a closure's signature, a `const fn` evaluated, an inlined callee at the optimized stage) | that function's step (`body_store::step_value`, as today) | the hash of its answer | none |
| a layout or drop glue the runtime passes read | the type's layout fact (`blaze_items::layout_hash`) | its hash | none |
| the dependencies as a whole | nothing: each read of one is one of the rows above, keyed in the dependency | | |

What makes rows 1 to 4 possible is that the body's own lowering is part of the step: the
capture (`body_store::capture`) must hold the reads `Body::of` made (its row's edges, which
the engine keeps), not the one read of `Body::of`; `Rule::OwnBody` and `Rule::Names` then
classify each as a row above. That is the engine piece to ask of cutoff-1112: a step's reads
flattened through named rows (`Reads::for_each_read` descending into `Body::of`,
`ExprScopes`, `body_upvars_mentioned` of the own body).

**The answer names items by item key**, never by a place among the body's resolutions:
`keys::def_key` gives the plain key for a path's target (`Rel::Target` goes; `Own`, `Closure`,
`Coroutine`, `Opaque` stay, they are places in the body's own text), and `def_of` resolves
with `blaze_items::key::resolve`. A renamed callee is then a changed name fact and a miss,
where today it is a hit: the price of a lookup that needs no body. Decoding reads the counts
the entry keeps (`read_in`, `BodyRef::nodes`) and no `Body::of`.

**The container is storage only.** A crate's entries are packed into one value (`pages`) when
the crate goes quiet (the seal), found there by the entry's key (base key and shape slot)
through the bucket table; pins and exports stay one unit a crate. An entry made since is its
own store value until the next pack. The crate's key (`runtime::key` of its filed image key)
is only the quick check: when it is what the pack was made under, every entry of the pack
holds and no fact is compared. When it is not, each entry asked for is checked by its facts,
so one function edited misses that function, and its callers only if what they read of it
(its signature; its body when a step read it) changed.

**Steps and stages.** One entry family, a step each: inference (with the verdict), runtime
MIR (the stage codegen reads; built MIR is no longer stored: nothing reads it that a hit does
not replace, and the borrow check's verdict is its own answer), promoteds and the coroutine
facts as parts of the runtime entry.

**Tests.** (1) a crate of N functions, one edited, every function asked: N-1 hits and 0
`Body::of`, `infer_body_query`, `mir_body_query` runs for them, 1 miss (control: the
crate's key as the entry's key: N misses); (2) a signature that one function calls changed:
exactly its callers miss; (3) a new item that shadows a name a body resolved (through a glob):
the bodies that looked the name up in that scope miss (control: negative name facts left out
of the shape: the stale answer is served, and the refinement against a database built from
source catches it); (4) the piece above's tests, kept.

**What cannot be had as asked, or is not known yet.** A body in a block has no anchor of its
own outside a walk of the body it is in (`placed_body_syntax`): it is keyed under the
enclosing body's entry, never alone. A body a macro writes: its content is the call's tokens
and the definition's; whether the expansion's hash can stand in (an expansion is made before
the body is lowered, and is cheaper than the lowering) is to settle with records-items. A
location macro's value is a read of the body's position and the file's name: recorded as
facts, it makes such a body miss on every move, which is right for `line!()` and the reason
the hole (slot 5) is better: both are needed. Not read yet by this lane, so the table's
"as today" rows rest on `reads.rs`'s rule names and module text, not on its arms:
`values.rs`, `names.rs`, `keys.rs` past its head.

**Agreed with the lanes beside this one (2026-11-12, by message; none of it written yet).**

- *The vocabulary of a read is `blaze_items::Fact`* (cutoff-1112: a unit's parts use the same),
  its value from `blaze_items::facts::value(db, seen_by, &fact)`. The table's rows in it: a
  name looked up, found or not, is `Fact::Resolves { scope, name }` ("nothing" is a value);
  an impl search `Fact::Implements { trait_, self_ty }`; a method lookup
  `Fact::Method { self_ty, name }`. Two kinds are missing and are added there, never as a
  second form here: a textual-scope (legacy) macro lookup, and the traits in scope of a
  module; cutoff lists them in store.md, "Early cutoff between crates".
- *The per-item source hash is `ItemSource`* (records-items): three position-free hashes an
  item, `attrs`, `header` (the item with every body cut) and `body`, in an index a file keyed
  by the file's content and edition (no parse for an unchanged file); a macro-written item's
  in an index keyed by the expansion's inputs (the outermost call's tokens, each definition's
  content hash, hygiene, cfg, edition), never by the expansion. An entry's base key takes
  `body` and `header`; a macro definition's content is its item's `header`. `body` is the hash
  of the item's own body node: `BodySyntax`'s `tree` is of the anchor, which its doc says is
  wider than the item for an item of an `impl` or module with attributes, so the container's
  `attrs` go into the owner part. Not checked in `anchored_body_syntax`'s code by either
  lane.
- *The descent is the engine's* (sealgroups): `Reads::for_each_read_through(storage, through,
  each)` beside `Reads::for_each_read`, yielding `RowRead::Read` or `RowRead::Stored`: a row
  under `through` gives its kept edges in its place; one answered from a record is yielded
  as `Stored` and never descended (its edges are its key's reads: the caller reads that
  record's shape); edges outlive an evicted value; a row not current in the view is refused
  by a panic naming it, so the step fetches `Body::of`, `body_expr_scopes` and
  `body_upvars_mentioned` before descending.

**What reading the code showed (2026-11-12, lane records-bodies2), and what it changes above.**

- *Positions.* An answer holds no position in a file: inference is by `ExprId`/`PatId`, a MIR
  span is an id of the body's store (`mir/persist.rs` `span`), a location literal is a slot.
  So a sibling method that changes length moves nothing in an entry, and where the item is
  in its anchor is in no key. What does move is `Body::unmoved_source_map`, which whoever
  wants a position reads (a diagnostic, codegen's panic `Location` and debug lines), and
  that is a lowering. The runtime entry therefore gets a positions part of its own: span to
  range relative to the body's own node, rebased when read. It comes right after the runtime
  step; until then a body with a panic location or debug lines is lowered by codegen.
- *The content hash* is the tree hash of the item's own node (`BodySyntax::item_hash`, to
  be kept as a field made in `anchored_body_syntax`; the same value as records-items'
  `ItemSource.body`), not `BodySyntax`'s `tree`, which `anchored_body_syntax` makes of the
  anchor (`kept(anchor)`): a whole `impl` for a method.
- *A macro-written body's key also names every macro of its call chain* (item key and the
  hash of its definition's tokens), for soundness and not only for hits: two expansions of
  equal tokens under different def-site hygiene look names up in different scopes, and the
  base key must fix which facts are read first (each later read follows from the values of
  the earlier ones).
- *Signatures leave the key.* The identity hashed every path's target by key and signature
  (`names.rs` `item_part`); now a signature is a fact only where a step read it, so a
  changed callee misses the bodies whose inference read its signature and no other.
- *The lowering is a step of its own, `Step::Lower`.* Four decoders read the body's store
  only for counts (`body_store/infer.rs` `Nodes`, `mir.rs` and `check_cache/runtime.rs`
  `Bounds`, `value.rs`), and a closure is interned by its place among its body's closures
  (`hir_ty::db::ClosureKey::new` and `loc` read `closure_exprs`, which opens the store):
  every body with a closure is lowered to intern or to place one. So the small facts of a
  lowered body are an answer: the counts, `closure_exprs`, `Body::has_lowering_errors`,
  whether it has location literals. Its shape holds the reads `Body::of` made (the descent);
  every other step lists one fact, "the lowering of the body itself" (as `STEP_INFER` is
  listed today), and its own direct reads. `closure_exprs` and the counts answer from it.
  Built so far of this: the inference answer keeps its `Nodes` (`InferRoot.nodes`), and a
  relative key of a closure carries its kind, so `keys::relative_def` opens no store.
- *The kinds of fact added* are the item store's own (`blaze_item_hash::values::kinds`, the
  form is `FactRef`; cutoff moves a unit's `Part::Fact` to `FactRef` and lists them in
  store.md), not `blaze_items::Fact` variants as the paragraph above says, and two, not
  four: `0x1806` `TRAITS_IN_SCOPE` (module: the keys of `module_trait_set`, sorted) and
  `0x1807` `WHOLE_SCOPE` (module: every name with its `Resolves` value; only for a real
  iteration, each counted by the reading query's name in the flush's report), then
  `STEP_LOWER`. A name looked up, found or not, stays `Fact::Resolves`, and so does a
  textual-scope macro lookup: `index::ScopeEntry::Module` holds the name's `legacy_macros`
  beside its three namespaces, each binding with its visibility and whether it came through
  a glob. A macro's definition is its `Signature` fact (`form.rs` `macro_`). Both written
  (`values.rs`: `traits_in_scope`, `whole_scope`, `resolves`; `value` answers them).
- *A read's key to (module, name)* needs `hir_def::nameres::name_read(db, read, kind) ->
  Option<NameRead>` with `NameRead::{Name, LegacyMacro, Traits, WholeScope, Children,
  Handle}` (`ScopeInput`, `ModuleScope` and `Modules` are private); the request, with the 21
  rows to answer for, is in `/tmp/records-1112e/next.md` section E. Not written: lane
  records-1112e ended; whoever gets there first writes it in `nameres.rs` and
  `nameres/frozen/pages.rs`.
- *Reads the steps make themselves through the resolver* are in their captures today and
  dropped by `Rule::Names`: they become facts with `name_read` alone. Only the reads inside
  `Body::of` wait for the descent.
- *Hard cases, decided.* A body with block-local items has no identity today and stays
  unstored (counted) until a block's scope has a key. A closure of another body named by
  plain key (`keys::closure_owner`) lowers that body: only revealed opaque types in runtime
  MIR reach it. A renamed callee misses its callers; two functions of the same text and
  different names no longer share an entry. Promoteds and the coroutine held-locals fact
  are parts of the runtime entry, keyed per instance where monowalk says the generic body
  does not give the held set.

**The switch and what deletes the old identity.** `identity_parts`' "hir", "entries" and
"names" parts (with `names.rs`, `entries.rs`, `slots.rs` and `portable_body`'s use here) are
deleted, with no switch left, when on one commit: `name_read` and the descent are in; a run
of reth and tempo lists no read in `Flushed::refused` that the old path stored; and the
count test (N-1 hits with 0 `body_query`, `infer_body_query`, `mir_body_query`) and the
refinement under random edits pass with their controls.

**State (2026-11-12, lane records-bodies2).** Written, not yet built (ready list
`/tmp/round2/ready/records-bodies2.txt`): answers name a path's target by plain item key and
`Rel::Target`, `target_indices` and `Names::targets` are gone (`keys.rs`, `names.rs`;
`IDENTITY_DOMAIN` 5 so no record of the old form is found); a closure's relative key holds
its kind (`check_cache/ids.rs` `closure_kind_code`, `closure_kind_of`, now public through
`body_store`); the inference answer keeps its node counts (schema "inference 2"). The two
fact kinds and `resolves` are written with their values; nothing classifies a read into
them yet. Left, in
order: `name_read`, the reads the steps
make themselves as facts (`reads.rs` `Rule::Names`), `BodySyntax::item_hash` and the early
base key (`identity.rs`), `Step::Lower` with the descent, the runtime stage as a step and
`check_cache`'s per-crate lookup deleted, the positions part, the driver's lines, the tests
of this subsection.

**State (2026-11-12, lane records-bodies3; letters are the lane's list a to f).** Written, not
yet built (ready list `/tmp/round2/ready/records-bodies3.txt`):

- *a, done.* `hir_def::nameres::name_read(db, read, kind) -> Option<NameRead>` (end of
  `nameres.rs`; helpers `delta::publish::name_read`, `frozen::pages::{read_of, paged_index,
  PagedRead}`). It takes the `ReadKind`: a struct's fields share one ingredient and are told
  apart by field. One variant more than planned, `NameRead::Crate(Crate)`: a `DefMap` is not
  a handle, it holds the prelude, the extern and `#[macro_use]` preludes and the root's flags
  (`no_std`, edition, registered tools), so `def_map`, `DefMapPair` and
  `crate_shadows_primitive_type` are reads of those, hashed by the new fact kind `0x1808`
  `CRATE_NAMES` (`values::crate_names`, over `hir_def::nameres::crate_names`; subject: the
  root module's key). Handles: `Modules`, `ModuleScope`, `Modules::module`, `table`,
  `crate_local_def_map`, `ModuleIdLt`, `BlockIdLt`, `row`, `CrateResolution::module`,
  `ScopeInput.module`. `Children` and `block_def_map` map to `WHOLE_SCOPE`.
- *b, done.* `reads::classify_with(.., Names::Facts)` turns `Rule::Names` rows into facts
  through `name_read` (`reads::name_fact`); `module_trait_set` is `Rule::TraitsInScope`; a
  whole-scope read is `Classified::Scan`, counted in `Flushed::scans` by (step, row). The
  switch is the type installed: `Bodies` (old, `Names::Identity`, still the only one the
  driver installs) or `blaze_item_hash::RecordedNames`. Rows added to `RULES` and to
  `identity::covered_queries` (refused before): `ScopeInput`'s `scope`, `children`,
  `module`; `Modules`' `table`; the paged memos `name_memo`, `legacy_macro_memo`,
  `scope_memo`, `traits_memo`. Tests in `blaze-item-hash/src/tests/classify.rs`:
  `a_name_fact_changes_when_the_name_is_supplied_or_shadowed_and_only_then`,
  `scope_and_crate_name_facts_change_with_what_they_hold_and_only_then`,
  `recorded_names_are_facts_with_values_or_handles`.
- *Not checked, to check on the first build:* that a paged scope's `module` index is the
  module's place in `DefMap::table` (`nameres::paged_module` panics naming it when not);
  that `crate_names` leaves nothing out a body step reads of a `DefMap`
  (`exported_derives`, `fn_proc_macro_mapping`, `derive_helpers_in_scope` are left out);
  that a module's `visibility` and `origin` in `ModuleData` reach no step except through
  the `Resolves` value of its name in its parent (the `Handle` claim).
- *c, first half done.* `BodySyntax::item_hash` (hir-def `expr_store/body.rs`): `tree_hash`
  of the item's own node, set in `anchored_body_syntax` (of `relative`) and
  `in_owner_body_syntax` (the item found in the owner's tree); not part of equality.
  It is NOT `ItemSource.body` (records-items2: `body` is the body node alone, `header` the
  item with every body cut): it is the whole item node, which is what the base key needs
  of both in one value. When `ItemSource` exists (its file index is unwritten), the base
  key may take `header` and `body` from it and this field goes; until then this is the
  one value. A macro definition's hash for the call chain:
  `hir_expand::declarative::stored::def_hash(db, AstId<ast::Macro>, def_crate) -> u128`
  (records-items2, written, uncompiled).
- *Next:* the paragraph below (lane records-bodies4 took over here).

**State (2026-11-12, lane records-bodies4; successor of records-bodies3, same letters).**
Written, not yet built (ready list `/tmp/round2/ready/records-bodies4.txt`):

- *c, done.* `identity::early_identity` / `early_parts` (blaze-item-hash `identity.rs`), what
  `RecordedNames::identity` returns; `Bodies` is unchanged. Four named parts: "crate inputs"
  (`crate_inputs` as it is: read, it hashes crate keys and settings, nothing a crate holds),
  "owner" (the kind of body, the own item's key, the container impl's or trait's key; no
  signature), "module chain", "source". The source of a body written in a file is its
  `ItemSource` (`hir_expand::declarative::stored::index::source`: `header` and `body`, text
  hashes, the whole item between them; `BodySyntax::item_hash` is deleted: a tree hash differs
  between a lazy and a parsed body). Of a body a macro wrote (`macro_chain`): each macro of the
  chain of calls by its crate's key, kind, edition and `stored::def_hash`, then the outermost
  call's node (`hir_def::expr_store::body::ast_node_hash`, or the item's `ItemSource` for a
  derive or attribute). Under `Names::Facts` the owner's own `Signature` and `Attrs` reads
  are facts (`reads.rs`, end of `apply`): the key hashes neither. Tests:
  `blaze-item-hash` `tests::early` (four).
- *No early identity (`Err`, counted `no_identity`), each to close:* an item of a block
  (keyed with its body: by design); a body a proc macro wrote whose expander is not
  loaded; a body an
  eager macro wrote (`include!` reads a file: needs that file's content as a part); a chain
  longer than 256.
- *Unchecked guesses, now five (5 is under the proc-macro item):* the predecessor's
  three (paged scope index = place in
  `DefMap::table`; `exported_derives`, `fn_proc_macro_mapping`, `derive_helpers_in_scope`
  left out of `CRATE_NAMES`; `ModuleData` visibility and origin as a handle), none touched;
  and 4: a macro 2.0 (`macro`) is named in a chain by crate key and definition hash, not by
  its defining module, which its def-site names resolve in: read how a `MacroDef` body's
  hygiene resolves (`hir_expand::hygiene`, `Transparency::Opaque`) and add the module's key
  to the chain if it matters.
- *Narrowing, later:* the key takes the whole item (`header` and `body`). Taking only the
  block, the parameter list and the attributes lowering reads is an optimisation to make
  only with a test that nothing else of the item is read.
- *Proc-macro-written bodies, written.* `macro_chain`'s `ProcMacro` arm: the macro crate's
  key, the ast id of the macro's function (`ErasedFileAstId::into_raw`: unchecked guess 5,
  that it is the same number in every database for the same file), and `expander_code`:
  every macro of the crate by name, kind and `expander.code()` (the shape of
  `hir-def/src/deps/key.rs` `libraries`); no early identity while the expander is not
  loaded. Not checked: whether `blaze_macro_eval::code::of` hashes paths (two checkouts
  would differ). Not done: counting the bodies with no early identity by reason in `Flushed`
  (a `Vec<(&'static str, u64)>` beside `no_identity`; `BodyFacts::identity` must then
  return the reason, `Result<ContentId, &'static str>`, a whole port of its implementors),
  and no test of this arm (the fixture's proc macros say no `code`).
- *`Step::Const = 3`, done* (cache-audit-1112's hunks: a constant's value is the fourth
  step, `body_store/konst.rs`, its file). Arrays by step are `[StepCounts; Step::COUNT]`
  in hir-ty; `blaze-cargo/src/serve/unit/deps.rs` and its tests name 4 (records-1112g's
  script, run). A `const fn` a constant calls is refused until the callee's code is a fact.
- *d, designed, not written.* (1) `Step::Lower = 4` (`body_store/facts.rs`: `Step::ALL` of
  five, `what`'s arm, `value.rs`'s arm; the arrays follow `Step::COUNT` but
  `blaze-cargo/src/serve/unit/deps.rs` names 4 and a fifth name, "lowering"). (2) A new `body_store/lower.rs`:
  a tracked `lowering(db, def) -> Lowered { exprs, pats, bindings, closures: Box<[ExprId]>,
  errors: bool, located: bool, value: Option<ContentId> }`, looked up as `Step::Lower` and on
  a miss made from `Body::of`, `closure_exprs`' filter, `Body::has_lowering_errors` and
  `location_literals`, under `capture`. (3) Its offer reads the capture with
  `Reads::for_each_read_through(storage, through, each)` where `through` is a new
  `BodyFacts::lowering_row(db, def, read, kind, name) -> bool` (blaze-item-hash: the rule is
  `Rule::OwnBody` and the key is `def`; `false` for `Bodies`); a `RowRead::Stored` refuses
  the offer by the row's name. The rows inside `Body::of` that are the item's own text
  (`ItemBodySyntax::syntax`, `placed_body_syntax`, `file_body_syntax`, the file's text and
  parse) need rules that say covered; the first run's `Flushed::refused` lists the rest.
  (4) The value of the fact "own lowering" (`STEP_LOWER`, a new kind in `values.rs` after
  `0x1808`; `step_value(db, def, Step::Lower)`) is NOT the hash of the answer (two bodies
  of equal counts differ in what their lowering read): it is the digest of what the lowering
  read, `AnswerKey::new(base)` fed the shape's fact values in order, the same on a hit
  (`Store::answer_key_now` has the values) and on a miss (`Store::offered` has
  `scratch.values`), independent of the slot. `Lowered::value` holds it; `None` when the
  offer was refused, so a step that read the lowering is refused too and counted.
  (5) `reads.rs` under `Names::Facts`: a read of `lowering` of `def`, and `Rule::OwnBody`
  and `Rule::OwnStore` of `def`, are `Classified::Fact(STEP_LOWER of OWN)`; each step's
  query fetches `lowering(db, def)` first so the fact has a value. (6) `closure_exprs`
  (hir-ty `db.rs`) answers from `lowering(db, def).closures` when the owner is a body and a
  store is installed; `Bounds`/`Nodes` at decode come from it (`body_store/mir.rs`
  `locations`: `Body::of` only when `located`; `check_cache/runtime.rs` `encode_entry`;
  `value.rs` `mir_hash` walks `closures`, not `store.exprs()`, and needs each closure's
  kind: keep it in `Lowered` beside the expression). A step's order: `reads.rs`
  `Rule::OwnStep` compares steps by number, so "lowering is earlier than every step" is a
  rule of its own there, not `Step::Lower < step`.
- *Next, exactly:* write d (1) to (4) in hir-ty as one whole piece with its test (a body
  whose macro's definition changes: equal counts, another `Lowered::value`; control: the
  value taken from the answer's bytes is equal), then (5), (6), the proc-macro arm, f1, e.
