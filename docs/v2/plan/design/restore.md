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

