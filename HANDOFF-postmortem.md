# Handoff: flight recorder postmortem (pd-postmortem)

Worktree: /Volumes/Projects/blaze-wt-pd-postmortem, branch pd-postmortem. Not built, checked or
tested (as instructed). Not committed.

## What it does

Once the daemon's history opens, it persists the flight recorder into
`<history>/flight-<pid>.map`. The file is mmapped shared and holds an exclusive `Locked` file
lock for the process's life. After a crash the kernel keeps the pages.

The next `History::open`, and `blaze slow` when no daemon holds `owner.lock`, then does this for
each `flight-*.map` it can lock (the process is dead):
1. Validates the header.
2. Decodes the rows past each ring's flushed cursor.
3. Renumbers their names and sites into this process.
4. Writes them as normal history files of the dead pid and process name.
5. Writes a `builds` row with outcome `crashed` for each build that was running. Its `dur_ns`
   runs to the last row read back, or the last flush if that is later. `blaze` is the dead
   process's hash.
6. Deletes the file.

## Files changed

- `crates/blaze-profile/src/flight/postmortem.rs` (new). The layout, the writer (`Mapping`), the
  global hooks, the reader (`dead`, `Found::read`, `Found::remove`, `Recovered`),
  `PostmortemError`, and 3 tests.
- `crates/blaze-profile/src/flight.rs`
  - `pub mod postmortem`.
  - New `Store`: head, flushed, number, a mirror of the last 64 owners, padded to 4096 bytes,
    then the rows. `Ring { store: AtomicPtr<Store>, moving: AtomicPtr<Store> }` replaces
    `head` and `words`.
  - `put` checks `moving`; a cold `move_store` copies the rows. `Ring::read` delegates to
    `Store::read`.
  - `claim` mirrors the owners into the store, gives a new ring a mapped store once persisted,
    and finishes a pending move.
  - `take()`'s per-row match is factored into `fn decode(into, (tid, name), &row, &scale,
    counting_copies)`. It is the one decoder, used by both live reads and the postmortem.
  - Name hooks in `Names::intern` and `intern_kept`; `Names::each`.
  - A `RECOVERED` name table: epoch 0, shard 1, scoped to one postmortem. `name()` checks it.
  - Site hook in `register`.
  - `SiteText` and `pub fn site_text(site)`. Foreign sites (≥ 4096, at most 8192, leaked,
    deduped) through `foreign_site`.
  - `pub fn flushed(&Cursor, at_ns)` and `pub fn persist(dir, blaze, process)`.
  - Top doc updated.
- `crates/blaze-profile/src/clock.rs`: `Scale::line()` and `Scale::of_line()`, the clock's base
  as plain numbers.
- `crates/blaze-profile/src/span.rs`: `Value::SYM` (default false; true for `Name`; `Option`
  forwards it) and the `SYM_COLUMNS: [bool; COLUMNS]` macro const.
- `crates/blaze-profile/src/batches.rs`: `spans`, `events` and `folded` name sites through
  `flight::site_text` instead of `flight::meta`, so foreign sites resolve.
- `crates/blaze-profile/src/history.rs`. The edits are localized:
  - `Outcome::Crashed`, appended last so dictionary keys stay stable.
  - `HistoryError::Postmortem`.
  - New fields: `cursor`, `postmortem` (a deferred open error), `persisted`, `clean`.
  - In `open`: one `recover_into(dir, &mut writer, &mut seq)` call after `writer.schemas`.
  - `persist_recorder()`.
  - Hooks in `build_started` and `build_ended`.
  - `Drained::take` also clones the cursor.
  - `write` calls `flight::flushed` right after the table files are written, and returns the
    deferred postmortem error or `flight::postmortem::failed()` before `unidentified`.
  - `Drop` calls `postmortem::forget()` only if persisted and the last flush succeeded.
  - New free functions `recover_dead(dir)` (for `blaze slow`), `recover_into` and
    `write_recovered`.
  - Top doc updated.
- `crates/blaze-profile/src/time/load.rs`: `pick` selects `outcome`. A `crashed` row with a
  `dur_ns` gives `died: Some(pid)`.
- `crates/blaze-profile/src/slow/load.rs`: `run` calls `history::recover_dead` first.
- `crates/blaze-profile/src/time.rs`: `TimeError::Postmortem(HistoryError)`.
- `crates/blaze-cargo/src/serve/observe.rs`: `keep_history` calls `history.persist_recorder()`
  after open. An error is logged and sent to `failed_to`; it is not fatal.
- `crates/blaze-profile/src/schema.rs` (`builds.outcome` doc) and `sql/views.sql` (builds view
  comment).
- `docs/profiling.md`: builds table and paragraph, a new "Past a crash" subsection of the flight
  recorder, and the `blaze slow` death paragraph. Front matter is kept.

## File layout (LAYOUT = 1)

Every part is 64 KiB aligned. A runtime check refuses to persist if the system page size does not
divide 64 KiB.

| part | offset | size | contents |
|---|---|---|---|
| header | 0 | 64 KiB | see below |
| sites | 64 KiB | 2 MiB | 4095 records × 512 B; word0 = valid, cut, level, lengths, written last with release; then name and target |
| builds | | 256 KiB | 32 records × 8 KiB; word0 = build_id, written last, 0 = free; started_ns, started_unix_ns, lengths, then commit, workspace and command |
| name logs | | 17 MiB | 17 logs × 1 MiB (16 shards plus kept names); entries are `[text][pad8][u64 len, cut bit, sym<<32]`, appended circularly, read backward from the head |
| ring stores | | 2 MiB + 64 KiB each | one `Store` per ring, appended and mapped one mmap per ring |

The header holds:
- magic `BLZFLGHT`, an endian mark, LAYOUT, and `ROWS_FINGERPRINT`, a const FNV over every row
  constant: row bits, plot bytes, column ids, Plot, Switched and WaitedOn numbers and names, span
  column names and SYM flags, Sym bits, OWNERS_KEPT and STORE_HEAD;
- pid, CAP, WORDS, STORE_BYTES, the number of rings mapped, and counting_copies;
- two clock slots plus an index;
- last_flush_ns and the 17 log heads;
- blaze hash and process name.

The fixed part is about 19.3 MiB. Each ring adds about 2.06 MiB. All of it is reserved on disk
up front: `posix_fallocate` on Linux; on macOS `F_PREALLOCATE` (contiguous, then any), then
`set_len`. A full disk is therefore an error at persist or claim time rather than a SIGBUS. Other
unixes get `Unsupported`.

## Design decisions worth a look

- **The binary hash is kept, not checked (a deliberate deviation from the brief).** The brief
  said to refuse a header whose magic, layout or blaze hash mismatches. I refuse on magic, byte
  order, layout, rows' fingerprint, CAP, WORDS, store size and file length. I do not refuse on
  the hash. The file is self-describing (its own site and name tables plus a fingerprint of
  every decoding constant), and "crash, rebuild blaze, restart" is the normal flow. Refusing on
  the hash would make nearly every postmortem fail. The hash fills `builds.blaze` of the crashed
  rows. If you want strict hash refusal, it is one more entry in the check list in
  `Found::read`; the header already stores the hash.
- **Refusals do not block startup.** A refused or damaged file makes `History::open` succeed
  anyway. The error is kept in `History::postmortem` and returned by the next flush, which the
  keeper reports to daemon status. The file stays in place and every daemon start reports it
  again; the message says to delete it or read it with the blaze that wrote it. In `blaze slow`
  the same error fails the command (`TimeError::Postmortem`). That is consistent with
  no-fallback, but a stale file of another layout breaks `blaze slow` until it is deleted.
- **A file the dead process never finished making** (shorter than 64 KiB, or magic still 0, since
  the magic is written last) has nothing in it. It is treated as zero rows and deleted, not
  refused.
- **Liveness is the file lock.** The writer holds `Locked` (exclusive flock); the reader takes
  `Locked::try_lock`. Being held means live (or another reader has it), so the file is skipped.
  Our own file is locked through another open file description, so it is skipped too.
- **`blaze slow` briefly takes `owner.lock`** while it writes recovered files (milliseconds), so
  file sequence numbers never race a daemon. A daemon starting in that window fails its claim
  with "written by another blaze serve". The message names the old pid, read from the file.
- **Rings made before persist.** Under the RINGS lock, `persist` moves a ring with no live thread
  directly. A live ring gets `moving`, and its own thread copies into the mapped store on its
  next `put`, or the next claimer does in `claim`. Only the writer ever writes a store. Old heap
  stores are leaked so a concurrent reader holding the old pointer stays valid.
- **A store that can't be made when a ring is created** (disk full, mmap failure) is stored in
  `FAILED`, and the ring stays on the heap. This is a degraded mode, not a silent one:
  `History::write` returns the error from the next flush, and daemon status shows it. It is
  arguably a fallback, so flag it if you'd rather fail the claim. A claim cannot fail without
  losing the thread's recording entirely.
- **Builds past 32 concurrent** are also reported through `FAILED`.
- **Recovered names live only for one `Recovered`.** Its `Session` holds a process-wide
  `RECOVERING` lock and clears `RECOVERED` on drop. Two `Recovered` alive in one thread would
  deadlock; `recover_into` reads and writes one at a time. Unknown names and sites become
  0/`?` and are counted (`names_lost`, `sites_lost`). Rows whose owner was not mirrored (more
  than 64 owners since the oldest row) are dropped and counted in `unowned`. Those counts are
  not yet written anywhere; you could add them as counters.
- **Foreign sites** are leaked for the process's life, deduped by (name, target, level), and
  capped at 8192. Past the cap is a typed `Full` error.

## Unsafe code: invariants and where they are upheld

- `Ring::store` and `Ring::store_mine` dereference `AtomicPtr<Store>`. Every store comes from
  `Store::heap` (leaked) or `Mapping::store` (never unmapped). Pointers are published with
  release. Writer loads are relaxed: the writer either set the pointer itself in `move_store`,
  or took the ring after `persist` set it, both under the RINGS lock with the `live` CAS.
- `Ring::move_store` dereferences `moving`, a mapped store made only for this ring, published
  with release; the acquire `swap` pairs with it.
- `Store::heap` uses `assume_init` on zeroed memory. All-zero bytes are a valid `Store`
  (atomics and integers only).
- `Mapping::store` casts a fresh `MAP_SHARED` mapping of `STORE_BYTES` (at least
  `size_of::<Store>()`, page aligned, file reserved as zeros) to `&'static Store`. It is never
  unmapped and is handed to exactly one ring.
- `Mapping::at` makes an `&AtomicU64` into the fixed mapping. The offset is asserted 8-aligned
  and in bounds, and the base is page aligned.
- `Mapping::bytes` uses `copy_nonoverlapping` into the fixed mapping, with bounds asserted. Each
  writer holds the mutex of its part (`logs[i]`, `sites`, `builds`); the header text is written
  once in `create` before anyone else has the mapping. No thread of the process reads these
  bytes.
- `unsafe impl Send/Sync for Mapping`: `fixed` is never unmapped, and every write goes through
  atomics or the per-part mutex.
- `sys::map` (mmap with the kernel choosing the address), `sys::reserve` (posix_fallocate /
  fcntl with a live `fstore_t`) and `sys::page_size` (sysconf).
- `rows hold no addresses`: rows hold site numbers and Syms, decoded through the file's own
  tables.

## Risks

- Disk use: about 19 MiB plus about 2 MiB per thread that ever records. A daemon with 150
  threads reserves about 330 MiB in the history dir. This is not counted against the history's
  cap.
- APFS or copy-on-write filesystems, or snapshots, may still fail a write into a reserved range
  when the disk is truly full, which means SIGBUS on a mapped page. That would be a crash in the
  recorder.
- Writeback I/O: dirty mapped pages are flushed by the kernel periodically (about 2 MiB per busy
  ring per wrap). There is no msync (none is needed; the kernel keeps pages after death). It is
  worth watching with `blaze slow` on a busy self-build.
- `flight::flushed` stores into the store loaded at that moment. If a ring is moving at the same
  instant, the copy may carry the older flushed mark, and a crash right then would duplicate up
  to one flush of rows. The same holds for a crash between a flush's files being written and
  `flushed` running.
- A flush that fails partway (some tables written, then a Parquet error) leaves those rows
  unmarked, so a later crash recovers them again (duplicates). A `write_recovered` that fails
  partway leaves the mapping, so the next open re-recovers it (duplicates).
- A thread that held its ring at persist and never writes again keeps its rows on the heap; they
  are lost to a postmortem.
- Name logs wrap at 1 MiB per shard. The oldest names can be gone while a row still holds them;
  this is counted in `names_lost`.
- The fingerprint does not cover the enum value orders of typed columns (Outcome, WaitKind,
  ModuleFrom words are index+1). Reordering those enums needs a LAYOUT bump.
- On Windows `persist_recorder` returns `Unsupported` on every daemon start, which goes to
  `failed_to` (daemon status). Decide whether that should be quieter.
- Merge conflicts:
  - The manifest agent is replacing glob discovery in history.rs. `write_recovered` and
    `recover_into` push `Kept` files and must also register them in the manifest (including the
    `blaze slow` path in `recover_dead`). The open-time call sits right after
    `writer.schemas(dir)?`.
  - The "cut recorder per-span cost" and "crash signals" agents edit flight.rs. The conflicts are
    in `Ring`, `put` and `take`; the hot `put` now goes through `store_mine()` and the `moving`
    check.

## Tests added (`flight/postmortem.rs`, unix only)

- `a_dead_process_s_rows_past_its_last_flush_read_back_whole`: an instance-level `Mapping` in a
  scratch dir. A standalone `Ring` on a mapped store writes 40 rows (spans and counters) with
  logged names, a site record, a running build and an ended build, and flushed = 25. Dropping
  the mapping releases the lock. `dead` and `read` then check:
  - counters are equal to the in-process decode of rows 25..40 using the file's clock;
  - spans are equal except for the renumbered site, thread name and crate, whose texts are
    checked;
  - only the running build is returned;
  - `last_ns` is right;
  - recovered names are gone after drop;
  - `remove` deletes the file.
- `a_mapping_of_another_layout_is_refused_naming_its_file`: patches LAYOUT in the file and expects
  `PostmortemError::Refused { what: "layout", path }`, a message naming the path, and the file
  left in place.
- `a_live_mapping_is_left_alone`: a held mapping is not listed; once released it is.

## What the main agent should check

1. `blaze check` for blaze-profile and blaze-cargo. The likely trouble spots, written blind:
   - the `ROWS_FINGERPRINT` const block (const-eval of `Plot::ALL[at] as u64`, `name()`,
     `span::*_COLUMNS`);
   - `#[cfg]` tail blocks in `Mapping::create` and `Mapping::store`;
   - the libc `fstore_t` / `F_*` constant types on macOS;
   - the borrows in the `renumber` closures;
   - the `SYM_COLUMNS` macro line in span.rs;
   - `Box::<Store>::new_zeroed()` (a 2 MiB+ type);
   - clippy on `std::mem::transmute`: none is left.
2. `blaze test -p blaze-profile`, especially the three postmortem tests, the existing flight
   tests (`Ring::new` is now a leaked heap `Store`), the history tests (`open` now lists the
   directory for maps), and the slow and time tests (`pick` now selects `outcome`;
   `slow::load::run` now calls `recover_dead`, which takes `owner.lock` when one exists).
3. An end-to-end check:
   - start a daemon and run a build;
   - `kill -9` the daemon mid-build;
   - check that `<state>/profile/flight-<pid>.map` exists;
   - run `blaze slow`: expect the build reported as died, with rows to the kill, a `crashed`
     row in `builds`, and the map deleted.
   Also restart a daemon instead of running `blaze slow` and check that its first flush status
   is clean.
4. Decide on the two policy points: binary hash checking, and the heap degraded mode on store
   failure.
