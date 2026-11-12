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
- **Still not restored: the front end itself.** The kept front end (`KeptLowered`: the dense
  `base_db` database with its def maps, inference and MIR of the members) lives in memory
  only. A restarted daemon lowers the workspace again and resolves its members again, reading
  dependencies from their images. Restoring it needs the front end's memos in the image as
  `Persist` values, the work [query-unify](../../plan.md) names.


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
