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
  - `restore the symbols`;
  - `restore the VFS`;
  - `open the database`;
  - `restore the plane's history`;
  - `install the library's trees`.

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
| syntax, item trees, macro expansions | about 550 MiB | library trees in the daemon image; expansions heap only |
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
| b2 | def maps and impl tables read in place; name resolution's state dropped for a sealed crate | 470 MiB less; under 2 GB | not built |
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
   one change still to make: each dependency enters as the content hash of its *interface
   record*, not as its key (content-keys-1112's rule for units: dependencies by interface
   digest). One key rule, one store (`blaze_cas`), the same key for a unit record's front-end
   part.
3. *Recorded.* Its records are in the store under that key: the interface (def map, lang
   items, signatures: today's image parts; impls by self type and trait are to be added), and
   by body key its bodies, inference and MIR (the item store).

A sealed crate is, to the engine, one leaf: an input `Seal(crate)` whose value is the content
hash of its interface record. **What a dependent's front end gets** is that interface record
read in place (names to items, signatures, impls), and bodies by body key when it asks
(generic and `#[inline]` functions, constants): a read of a body is a read of that body's
record, so a body edit below reaches the dependents that read that body and no other.

**What unseals one.** A write to any of its inputs, at once, in that write: the write finds
the crate from the input (file to source root to crates: `base_db::source_root_crates`; a
crate's own fields name it), with no walk of rows, as `hir_def::deps::withdraw_moved` does
today for a served crate's image. Only that crate is unsealed. Its dependents stay sealed
while its interface record's hash is the same after the edit (a body edit), and are unsealed
by the write of `Seal(crate)` when it is not (a signature edit): that write is the host's,
after the edited crate's interface is made again, before any dependent is asked.

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

**The engine change this needs** (not built; it is the reason the two tests below are not
written): three kinds for the whole database cannot say "only it". A write of kind `HIGH`
makes every `HIGH` row verify again, so unsealing one crate today costs a walk over every
sealed crate. The kind becomes a *seal group* a crate, ordered by the crate graph: a row keeps
its crate's group (16 bits of `Header::meta`), the storage keeps a view a group
(`closure_changed[g]`: the last view a leaf of `g` or of a group `g` depends on was written
in), and a row verified at or after its group's view is current with one compare, as a
durable row is now. A write to an input of crate `x` stamps `x`'s group and, only when
`Seal(x)` is written, each group above it: a loop over crates, no rows. A row that reads a
body below its own crate's interface records the body's group as well (a second word), so
the compare stays exact. Then a sealed crate's rows need no edges (they verify by the group)
and no place in the reverse index: the 108 and 106 MiB of step b1's second half.

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

**One front end a checkout (F1, above): not built, and it comes before the rest of this
section.** Measured by the owner on hello-world: `build`, then `test`, then `build` costs
about 4.5 s at each switch (`lower workspace` 3.6 s, `maintain` 2.7 s, 820,026 queries run,
5,087 inputs written), because `unkept` lets the front end go when the plan's units differ.
The invariant to build to: *a request's units are a view over one crate graph a checkout; a
switch of goal writes no input of a crate both plans hold, and runs a number of queries
bounded by the crates the new plan adds.* What `blaze_lower::lower_writes` makes a plan and
has to find-or-make in the kept `Lowered` instead:

| Made per plan now | Kept across plans by |
|---|---|
| source root ids, `0..sets` in the plan's order of package sets, and `other` | the load's directory (std's root, each package's) |
| `LibraryRoots`, `LocalRoots` (`::new`, a second call panics) | set when a root is added |
| `made`: a `Crate` a distinct (root file, data, cfg, env, runs, spelling) | the same tuple, with runs by run class unit id, not unit index; `Crate::create` only on a miss |
| `all_crates` (set to the plan's) | the union; a plan's own list in `Lowered` (`Crates::units`), which every `job.crates.krate(unit)` already goes through |
| `Lowered::{applied, class_runs, identities, narrowed, appended}` by unit index | by run class unit id and by `Crate` |
| `ProcMacros::give(NotYetBuilt)` for every proc-macro crate | only for a crate made by this plan |
| const macros' crates, macro hosts, doctests' crates | find-or-make by their file and content |
| `KeptLowered::{units, session}` compared whole (`unkept`) | `units` no longer compared; the session's keys by `Crate` stay |

and below it: a crate added to the graph (`Written::crates`) must give the name-resolution
and impls maintainers a job for the added crates alone (`delta::unreached` returns false for
every crate when `written.crates` is set), so the write names the crates it added. Its
checks: a revision case `build, test, build, test, run, check` whose every switch after the
first of each goal runs no query of a crate both plans hold (the engine's `Counts::executed`
by crate) and writes no shared input, at two sizes of the dependency; the control makes the
units part of the kept front end's identity again (`unkept`'s first test put back).
